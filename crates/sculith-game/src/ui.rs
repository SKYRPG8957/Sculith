//! Session-bound UI state; protocol adapters never own pending requests.
use super::*;
use sculith_core::{CustomForm, FormValue};

#[derive(Debug)]
pub(super) struct PendingForm {
    id: u32,
    plugin: String,
    request_id: u32,
    content: PendingContent,
    shown: Instant,
}

#[derive(Debug)]
pub(super) enum PendingContent {
    Simple { buttons: u32 },
    Custom(CustomForm),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormSubmission {
    pub player_id: u64,
    pub plugin: String,
    pub request_id: u32,
    pub selection: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomFormSubmission {
    pub player_id: u64,
    pub plugin: String,
    pub request_id: u32,
    pub values: Option<Vec<FormValue>>,
}

#[derive(Debug)]
pub enum UiSubmission {
    Simple(FormSubmission),
    Custom(CustomFormSubmission),
}

impl GameState {
    pub fn show_form(
        &mut self,
        id: u64,
        plugin: String,
        request_id: u32,
        form: &sculith_core::SimpleForm,
        now: Instant,
    ) -> Result<u32> {
        form.validate().map_err(anyhow::Error::msg)?;
        self.begin_form(
            id,
            plugin,
            request_id,
            PendingContent::Simple {
                buttons: form.buttons.len() as u32,
            },
            now,
        )
    }

    fn begin_form(
        &mut self,
        id: u64,
        plugin: String,
        request_id: u32,
        content: PendingContent,
        now: Instant,
    ) -> Result<u32> {
        ensure!(
            !plugin.is_empty()
                && plugin.len() <= 64
                && plugin
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid form owner"
        );
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        ensure!(
            player.pending_form.is_none(),
            "player already has a pending form"
        );
        let remaining = player
            .forms
            .available(now, 1.0, 1.0)
            .ok_or_else(|| anyhow::anyhow!("form clock regression"))?;
        ensure!(remaining >= 1.0, "player form rate exceeded");
        let form_id = self.next_form_id;
        self.next_form_id = form_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("form identifiers exhausted"))?;
        player.forms = Budget {
            remaining: remaining - 1.0,
            updated: now,
        };
        player.pending_form = Some(PendingForm {
            id: form_id,
            plugin,
            request_id,
            content,
            shown: now,
        });
        Ok(form_id)
    }

    pub fn submit_form(
        &mut self,
        id: u64,
        form_id: u32,
        selection: Option<u32>,
        now: Instant,
    ) -> Result<FormSubmission> {
        let player = self.form_player(id, form_id, now)?;
        let PendingContent::Simple { buttons } =
            &player.pending_form.as_ref().expect("validated").content
        else {
            anyhow::bail!("form expects input values, not a choice");
        };
        ensure!(
            selection.is_none_or(|choice| choice < *buttons),
            "form choice out of range"
        );
        let pending = player.pending_form.take().expect("validated above");
        Ok(FormSubmission {
            player_id: id,
            plugin: pending.plugin,
            request_id: pending.request_id,
            selection,
        })
    }
    fn form_player(&mut self, id: u64, form_id: u32, now: Instant) -> Result<&mut Player> {
        let player = self
            .players
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown player"))?;
        ensure!(player.snapshot.initialized, "player is not initialized");
        let pending = player
            .pending_form
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no pending form"))?;
        ensure!(
            pending.id == form_id,
            "form identifier does not belong to this session"
        );
        let elapsed = now
            .checked_duration_since(pending.shown)
            .ok_or_else(|| anyhow::anyhow!("form clock regression"))?;
        if elapsed > Duration::from_secs(60) {
            player.pending_form.take();
            anyhow::bail!("form reply expired");
        }
        Ok(player)
    }

    pub fn show_custom_form(
        &mut self,
        id: u64,
        plugin: String,
        request_id: u32,
        form: CustomForm,
        now: Instant,
    ) -> Result<u32> {
        form.validate().map_err(anyhow::Error::msg)?;
        self.begin_form(id, plugin, request_id, PendingContent::Custom(form), now)
    }

    pub fn submit_custom_form(
        &mut self,
        id: u64,
        form_id: u32,
        values: Vec<FormValue>,
        now: Instant,
    ) -> Result<CustomFormSubmission> {
        let player = self.form_player(id, form_id, now)?;
        let PendingContent::Custom(form) =
            &player.pending_form.as_ref().expect("validated").content
        else {
            anyhow::bail!("form expects a choice, not input values");
        };
        form.validate_values(&values).map_err(anyhow::Error::msg)?;
        let pending = player.pending_form.take().expect("validated");
        Ok(CustomFormSubmission {
            player_id: id,
            plugin: pending.plugin,
            request_id: pending.request_id,
            values: Some(values),
        })
    }

    /// Native cancellation does not identify the form kind. Attribute it from
    /// the owner-held request, never from a client-supplied discriminant.
    pub fn cancel_form(&mut self, id: u64, form_id: u32, now: Instant) -> Result<UiSubmission> {
        let player = self.form_player(id, form_id, now)?;
        let pending = player.pending_form.take().expect("validated");
        Ok(match pending.content {
            PendingContent::Simple { .. } => UiSubmission::Simple(FormSubmission {
                player_id: id,
                plugin: pending.plugin,
                request_id: pending.request_id,
                selection: None,
            }),
            PendingContent::Custom(_) => UiSubmission::Custom(CustomFormSubmission {
                player_id: id,
                plugin: pending.plugin,
                request_id: pending.request_id,
                values: None,
            }),
        })
    }
}
