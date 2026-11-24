use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BufMut;
use pumpkin_data::meta_data_type::MetaDataType;
use pumpkin_data::packet::CURRENT_MC_VERSION;
use pumpkin_data::tracked_data::{TrackedData, TrackedId, living_entity};
use sculith_protocol::java::client::play::{Metadata, MetadataSerializer};
use sculith_protocol::ser::WritingError;
use sculith_util::version::JavaMinecraftVersion;

pub trait ErasedSerializer: Send + Sync {
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError>;

    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8>;
}

struct SerializerHolder<T> {
    value: T,
}

impl<T: MetadataSerializer + Clone + Send + Sync + 'static> ErasedSerializer
    for SerializerHolder<T>
{
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let meta = Metadata::new_raw(index, r#type, &self.value);
        meta.write(writer, version)
    }

    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8> {
        let mut buf = Vec::new();
        let meta = Metadata::new_raw(index, r#type, &self.value);
        let _ = meta.write(&mut buf, &JavaMinecraftVersion::V_26_3);
        buf
    }
}

pub struct DataItem {
    pub tracked: TrackedData,
    pub serializer: Box<dyn ErasedSerializer>,
    pub canonical_bytes: Vec<u8>,
    pub dirty: bool,
    pub is_default: bool,
}

impl DataItem {
    fn new<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        tracked: TrackedData,
        value: T,
        is_default: bool,
    ) -> Self {
        let holder = SerializerHolder { value };
        Self {
            tracked,
            canonical_bytes: holder.write_canonical(tracked.id, tracked.r#type),
            serializer: Box::new(holder),
            dirty: !is_default,
            is_default,
        }
    }
}

pub struct SynchedEntityData {
    items: Mutex<HashMap<TrackedData, DataItem>>,
    is_dirty: AtomicBool,
}

impl Default for SynchedEntityData {
    fn default() -> Self {
        Self::new()
    }
}

impl SynchedEntityData {
    /// An empty tracker; values are added by [`Self::define`] and [`Self::set`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            is_dirty: AtomicBool::new(false),
        }
    }

    /// Registers a tracked value with the default the client already assumes, so it
    /// is only sent once something actually sets it.
    pub fn define<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        tracked: TrackedData,
        value: T,
    ) {
        let item = DataItem::new(tracked, value, true);
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        items.insert(tracked, item);
    }

    /// Stores a tracked value and marks it dirty, returning whether it differs from
    /// the value that was already stored.
    pub fn set<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        tracked: TrackedData,
        value: T,
    ) -> bool {
        let holder = SerializerHolder { value };
        let new_canonical = holder.write_canonical(tracked.id, tracked.r#type);

        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(item) = items.get_mut(&tracked) {
            if item.canonical_bytes == new_canonical {
                return false;
            }
            item.canonical_bytes = new_canonical;
            item.serializer = Box::new(holder);
            item.dirty = true;
            item.is_default = false;
        } else {
            items.insert(
                tracked,
                DataItem {
                    tracked,
                    serializer: Box::new(holder),
                    canonical_bytes: new_canonical,
                    dirty: true,
                    is_default: false,
                },
            );
        }
        self.is_dirty.store(true, Ordering::Release);
        true
    }

    /// Whether any value changed since the last [`Self::clear_dirty`].
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.is_dirty.load(Ordering::Acquire)
    }

    /// Serializes the values that changed since the last [`Self::clear_dirty`], or
    /// `None` when nothing changed.
    pub fn pack_dirty_for_version(&self, version: &JavaMinecraftVersion) -> Option<Box<[u8]>> {
        if !self.is_dirty.load(Ordering::Acquire) {
            return None;
        }

        let items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::pack_values(items.values().filter(|item| item.dirty), *version)
    }

    /// Takes one publication batch and records whether it contains the watched value.
    pub(super) fn take_dirty_for_version(
        &self,
        version: JavaMinecraftVersion,
        watched: TrackedData,
    ) -> (Option<Box<[u8]>>, bool) {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let watched_dirty = items.get(&watched).is_some_and(|item| item.dirty);
        let bytes = Self::pack_values(items.values().filter(|item| item.dirty), version);
        self.clear_items(&mut items);
        (bytes, watched_dirty)
    }

    /// Commits the changed effect and health fields after the sender accepts the batch.
    /// Fields whose publication failed stay dirty for the ordinary tracker pass.
    pub(super) fn try_publish_effect_data<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        particles: Option<(T, Option<bool>)>,
        health: Option<f32>,
        publish: impl FnOnce(Option<Box<[u8]>>) -> Option<bool>,
    ) -> bool {
        let (particles, ambient) = particles.map_or((None, None), |(particles, ambient)| {
            (
                Some(DataItem::new(
                    living_entity::EFFECT_PARTICLES,
                    particles,
                    false,
                )),
                ambient,
            )
        });
        self.try_publish_items(
            [
                particles,
                ambient.map(|ambient| {
                    DataItem::new(living_entity::EFFECT_AMBIENCE_ID, ambient, false)
                }),
                health.map(|health| DataItem::new(living_entity::DATA_HEALTH_ID, health, false)),
            ]
            .into_iter()
            .flatten(),
            publish,
        )
    }

    pub(super) fn try_publish_value<T: MetadataSerializer + Clone + Send + Sync + 'static>(
        &self,
        tracked: TrackedData,
        value: T,
        publish: impl FnOnce(Option<Box<[u8]>>) -> Option<bool>,
    ) -> bool {
        self.try_publish_items([DataItem::new(tracked, value, false)], publish)
    }

    fn try_publish_items(
        &self,
        changes: impl IntoIterator<Item = DataItem>,
        publish: impl FnOnce(Option<Box<[u8]>>) -> Option<bool>,
    ) -> bool {
        let Ok(mut items) = self.items.try_lock() else {
            return false;
        };
        let changes: Vec<_> = changes
            .into_iter()
            .filter(|change| {
                items.get(&change.tracked).is_none_or(|current| {
                    current.dirty || current.canonical_bytes != change.canonical_bytes
                })
            })
            .collect();
        let bytes = Self::pack_values(changes.iter(), CURRENT_MC_VERSION);
        let Some(delivered) = publish(bytes) else {
            return false;
        };
        for mut change in changes {
            change.dirty = !delivered;
            items.insert(change.tracked, change);
        }
        self.is_dirty
            .store(items.values().any(|item| item.dirty), Ordering::Release);
        true
    }

    fn pack_values<'a>(
        items: impl Iterator<Item = &'a DataItem>,
        version: JavaMinecraftVersion,
    ) -> Option<Box<[u8]>> {
        let mut buf = Vec::new();
        let mut has_any = false;

        for item in items {
            let before_len = buf.len();
            if item
                .serializer
                .write(item.tracked.id, item.tracked.r#type, &mut buf, &version)
                .is_ok()
                && buf.len() > before_len
            {
                has_any = true;
            }
        }

        if !has_any {
            return None;
        }

        buf.put_u8(255);
        Some(buf.into_boxed_slice())
    }

    /// Marks every value as sent.
    pub fn clear_dirty(&self) {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.clear_items(&mut items);
    }

    fn clear_items(&self, items: &mut HashMap<TrackedData, DataItem>) {
        for item in items.values_mut() {
            item.dirty = false;
        }
        self.is_dirty.store(false, Ordering::Release);
    }

    /// Serializes every value that differs from the default the client assumes, or
    /// `None` when they are all still at their default.
    pub fn get_non_default_values_for_version(
        &self,
        version: &JavaMinecraftVersion,
    ) -> Option<Box<[u8]>> {
        let items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::pack_values(items.values().filter(|item| !item.is_default), *version)
    }
}

#[cfg(test)]
mod test {
    use pumpkin_data::tracked_data::player::PLAYER_MODE_CUSTOMISATION;
    use sculith_protocol::codec::var_int::VarInt;

    use super::*;

    #[test]
    fn regional_effect_publication_preserves_other_dirty_fields() {
        let data = SynchedEntityData::new();
        let particles = living_entity::EFFECT_PARTICLES;
        let ambient = living_entity::EFFECT_AMBIENCE_ID;
        data.set(PLAYER_MODE_CUSTOMISATION, 0x7fu8);
        data.set(particles, VarInt(0));
        data.set(ambient, false);
        let before = data.pack_dirty_for_version(&CURRENT_MC_VERSION);

        assert!(!data.try_publish_effect_data(Some((VarInt(0), Some(true))), Some(7.0), |_| None));
        assert_eq!(data.pack_dirty_for_version(&CURRENT_MC_VERSION), before);

        let mut expected = DataItem::new(particles, VarInt(0), false).canonical_bytes;
        expected.extend(DataItem::new(ambient, true, false).canonical_bytes);
        let health = living_entity::DATA_HEALTH_ID;
        expected.extend(DataItem::new(health, 7.0f32, false).canonical_bytes);
        expected.push(255);
        assert!(
            data.try_publish_effect_data(Some((VarInt(0), Some(true))), Some(7.0), |bytes| {
                assert_eq!(bytes.as_deref(), Some(expected.as_slice()));
                Some(true)
            })
        );
        let items = data.items.lock().unwrap();
        assert!(items[&PLAYER_MODE_CUSTOMISATION].dirty);
        assert!(!items[&particles].dirty && !items[&ambient].dirty && !items[&health].dirty);
        drop(items);

        assert!(
            data.try_publish_effect_data(Some((VarInt(0), Some(false))), Some(8.0), |_| Some(
                false
            ))
        );
        let items = data.items.lock().unwrap();
        assert!(items[&PLAYER_MODE_CUSTOMISATION].dirty && items[&ambient].dirty);
        assert!(!items[&particles].dirty && items[&health].dirty);
    }

    /// Metadata ids no longer vary per version: core serializes the current format for
    /// every client and `pumpkin-java-multiversion` translates. So the senders need no
    /// version check of their own to decide whether a value reaches a client.
    #[test]
    fn a_set_value_is_serialized_for_every_version() {
        let data = SynchedEntityData::new();
        data.define(PLAYER_MODE_CUSTOMISATION, 0u8);
        assert!(data.set(PLAYER_MODE_CUSTOMISATION, 0x7Fu8));

        let current = data.get_non_default_values_for_version(&JavaMinecraftVersion::V_26_3);
        let legacy = data.get_non_default_values_for_version(&JavaMinecraftVersion::V_1_20_5);
        assert!(current.is_some());
        assert_eq!(current, legacy);
        assert_eq!(
            data.pack_dirty_for_version(&JavaMinecraftVersion::V_1_20_5),
            current
        );
    }

    /// A value still at the default the client assumes is not sent.
    #[test]
    fn default_values_are_not_serialized() {
        let data = SynchedEntityData::new();
        data.define(PLAYER_MODE_CUSTOMISATION, 0u8);

        assert!(
            data.get_non_default_values_for_version(&JavaMinecraftVersion::V_26_3)
                .is_none()
        );
    }
}
