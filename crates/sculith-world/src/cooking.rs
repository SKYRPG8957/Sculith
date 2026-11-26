use pumpkin_data::recipes::{CookingRecipe, CookingRecipeKind};

/// Cooking duration on the shared Bedrock schedule. Fast appliances run at twice the rate.
#[must_use]
pub fn recipe_ticks(recipe: &CookingRecipe, appliance: &CookingRecipeKind) -> i32 {
    match appliance {
        CookingRecipeKind::Blasting | CookingRecipeKind::Smoking => (recipe.cookingtime / 2).max(1),
        _ => recipe.cookingtime,
    }
}
