use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pricing {
    pub prompt: String,
    pub completion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<u64>,
    pub pricing: Pricing,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCache {
    pub fetched_at_unix_seconds: u64,
    pub models: Vec<ModelInfo>,
}

impl ModelCache {
    pub fn is_fresh(&self) -> bool {
        now_unix_seconds().saturating_sub(self.fetched_at_unix_seconds) < 60 * 60 * 24
    }
}

impl ModelInfo {
    pub fn title(&self) -> String {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.id.clone())
    }

    pub fn is_free(&self) -> bool {
        price_is_zero(&self.pricing.prompt) && price_is_zero(&self.pricing.completion)
    }

    pub fn context_label(&self) -> String {
        match self.context_length {
            Some(length) if length >= 1_000_000 => {
                format!("{:.1}M", length as f64 / 1_000_000.0)
            }
            Some(length) if length >= 1_000 => format!("{:.0}k", length as f64 / 1_000.0),
            Some(length) => length.to_string(),
            None => "unknown".to_string(),
        }
    }

    pub fn cost_label(&self) -> String {
        if self.is_free() {
            "free".to_string()
        } else if self.pricing.prompt == self.pricing.completion {
            format!("${}/M tok", format_price(&self.pricing.prompt))
        } else {
            format!(
                "in ${}/M, out ${}/M",
                format_price(&self.pricing.prompt),
                format_price(&self.pricing.completion)
            )
        }
    }

    pub fn matches_filter(&self, filter: &str) -> bool {
        if filter.trim().is_empty() {
            return true;
        }

        let needle = filter.to_ascii_lowercase();
        self.id.to_ascii_lowercase().contains(&needle)
            || self.title().to_ascii_lowercase().contains(&needle)
            || self
                .description
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&needle)
    }
}

pub fn sort_and_filter_models(mut models: Vec<ModelInfo>, include_paid: bool) -> Vec<ModelInfo> {
    models.retain(|model| include_paid || model.is_free());
    models.sort_by(|left, right| {
        right
            .is_free()
            .cmp(&left.is_free())
            .then_with(|| {
                left.title()
                    .to_ascii_lowercase()
                    .cmp(&right.title().to_ascii_lowercase())
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

pub fn select_model(models: &[ModelInfo], preferred: Option<&str>) -> Option<String> {
    if let Some(preferred) = preferred {
        if let Some(model) = models.iter().find(|model| model.id == preferred) {
            return Some(model.id.clone());
        }
    }

    models.first().map(|model| model.id.clone())
}

pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn format_price(value: &str) -> String {
    match value.trim().parse::<f64>() {
        Ok(number) if number.fract() == 0.0 => format!("{number:.0}"),
        Ok(number) => {
            let formatted = format!("{number:.6}");
            formatted
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string()
        }
        Err(_) => value.trim().to_string(),
    }
}

fn price_is_zero(value: &str) -> bool {
    match value.trim().parse::<f64>() {
        Ok(number) => number == 0.0,
        Err(_) => matches!(value.trim(), "0" | "0.0" | "0.00"),
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelInfo, Pricing, select_model, sort_and_filter_models};

    fn sample_model(id: &str, prompt: &str, completion: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: None,
            context_length: Some(128_000),
            pricing: Pricing {
                prompt: prompt.to_string(),
                completion: completion.to_string(),
            },
            description: None,
        }
    }

    #[test]
    fn free_filter_keeps_only_zero_priced_models() {
        let models = vec![
            sample_model("free-model", "0", "0"),
            sample_model("paid-model", "0.3", "0.6"),
        ];

        let filtered = sort_and_filter_models(models, false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "free-model");
    }

    #[test]
    fn preferred_model_is_selected_when_available() {
        let models = vec![sample_model("a", "0", "0"), sample_model("b", "0", "0")];
        let selected = select_model(&models, Some("b"));
        assert_eq!(selected.as_deref(), Some("b"));
    }
}
