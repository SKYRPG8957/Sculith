use pumpkin_data::{
    item::Item,
    recipes::{CookingRecipeKind, get_cooking_recipe_with_ingredient},
};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};

pub(super) fn write_metadata(
    saved: &NbtCompound,
    entity: &mut NbtCompound,
    kind: &CookingRecipeKind,
    input: Option<&Item>,
) -> Result<(), String> {
    let duration = nonnegative_short(saved, "BurnDuration")?;
    let remaining = nonnegative_short(saved, "BurnTime")?;
    let progress = nonnegative_short(saved, "CookTime")?;
    match saved.get("StoredXPInt") {
        None | Some(NbtTag::Int(0)) => {}
        Some(NbtTag::Int(1..=i32::MAX)) => {
            return Err("stored Bedrock cooking experience cannot be converted".into());
        }
        _ => return Err("invalid Bedrock cooking experience field".into()),
    }
    if remaining > duration {
        return Err("remaining fuel time exceeds the Bedrock burn duration".into());
    }
    let total = input
        .and_then(|item| get_cooking_recipe_with_ingredient(item, kind.clone()))
        .map(|recipe| i16::try_from(crate::cooking::recipe_ticks(recipe, kind)))
        .transpose()
        .map_err(|_| "cooking recipe duration is too long")?
        .unwrap_or(0);
    if progress != 0 && progress >= total {
        return Err("saved Bedrock cooking progress does not match a supported recipe".into());
    }
    entity.put_short("lit_total_time", duration);
    entity.put_short("lit_time_remaining", remaining);
    entity.put_short("cooking_total_time", total);
    entity.put_short("cooking_time_spent", progress);
    Ok(())
}

fn nonnegative_short(saved: &NbtCompound, name: &str) -> Result<i16, String> {
    match saved.get(name) {
        None => Ok(0),
        Some(NbtTag::Short(value)) if *value >= 0 => Ok(*value),
        _ => Err(format!("invalid Bedrock cooking field {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_native_cook_and_fuel_timers_with_the_shared_recipe() {
        for (kind, input, duration, remaining, progress, total) in [
            (
                CookingRecipeKind::Smelting,
                &Item::RAW_IRON,
                1600,
                1573,
                28,
                200,
            ),
            (
                CookingRecipeKind::Blasting,
                &Item::RAW_GOLD,
                800,
                774,
                27,
                100,
            ),
            (CookingRecipeKind::Smoking, &Item::POTATO, 800, 775, 26, 100),
        ] {
            let mut saved = NbtCompound::new();
            saved.put_short("BurnDuration", duration);
            saved.put_short("BurnTime", remaining);
            saved.put_short("CookTime", progress);
            saved.put_int("StoredXPInt", 0);
            let mut entity = NbtCompound::new();
            write_metadata(&saved, &mut entity, &kind, Some(input)).unwrap();
            assert_eq!(entity.get_short("lit_total_time"), Some(duration));
            assert_eq!(entity.get_short("lit_time_remaining"), Some(remaining));
            assert_eq!(entity.get_short("cooking_time_spent"), Some(progress));
            assert_eq!(entity.get_short("cooking_total_time"), Some(total));
        }
    }

    #[test]
    fn rejects_cooking_state_that_cannot_be_resumed() {
        let mut saved = NbtCompound::new();
        saved.put_int("StoredXPInt", 4);
        let mut entity = NbtCompound::new();
        assert!(write_metadata(&saved, &mut entity, &CookingRecipeKind::Smelting, None).is_err());
        saved.put_int("StoredXPInt", 0);
        saved.put_short("BurnTime", 10);
        assert!(write_metadata(&saved, &mut entity, &CookingRecipeKind::Smelting, None).is_err());
        saved.put_short("BurnDuration", 20);
        saved.put_short("CookTime", 1);
        assert!(write_metadata(&saved, &mut entity, &CookingRecipeKind::Smelting, None).is_err());
        assert!(entity.is_empty());
    }
}
