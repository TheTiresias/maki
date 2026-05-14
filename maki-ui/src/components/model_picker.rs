use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use maki_providers::ModelTier;
use maki_providers::dynamic;
use maki_providers::provider::ProviderKind;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::theme;

const TITLE: &str = " Models ";

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.form_hint),
        Span::styled("  Alt+1 ", t.keybind_key),
        Span::styled("(set strong)", t.form_hint),
        Span::styled(" / ", t.form_hint),
        Span::styled("Alt+2 ", t.keybind_key),
        Span::styled("(set medium)", t.form_hint),
        Span::styled(" / ", t.form_hint),
        Span::styled("Alt+3 ", t.keybind_key),
        Span::styled("(set weak)", t.form_hint),
    ])
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char('1') => Some(ModelTier::Strong),
        KeyCode::Char('2') => Some(ModelTier::Medium),
        KeyCode::Char('3') => Some(ModelTier::Weak),
        _ => None,
    }
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    Close,
}

struct ModelEntry {
    spec: String,
    label: String,
    provider_display: &'static str,
    tier: String,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.tier)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display)
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    copilot_endpoints: Arc<ArcSwapOption<HashMap<String, String>>>,
    last_spec_count: usize,
}

impl ModelPicker {
    pub fn new(
        models: Arc<ArcSwapOption<Vec<String>>>,
        copilot_endpoints: Arc<ArcSwapOption<HashMap<String, String>>>,
    ) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
            models,
            copilot_endpoints,
            last_spec_count: 0,
        }
    }

    pub fn open(&mut self, current_spec: &str) {
        let guard = self.models.load();
        let specs = guard.as_deref();
        let endpoints = self.copilot_endpoints.load();
        let ep_map = endpoints.as_deref();
        self.last_spec_count = specs.map_or(0, Vec::len);
        let entries: Vec<ModelEntry> = specs
            .map(|s| {
                s.iter()
                    .filter_map(|s| parse_model_entry(s, ep_map))
                    .collect()
            })
            .unwrap_or_default();
        let current_idx = entries
            .iter()
            .position(|e| e.spec == current_spec)
            .unwrap_or(0);
        self.picker.open(entries, TITLE);
        self.picker.select(current_idx);
    }

    fn try_refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.models.load();
        let spec_count = guard.as_deref().map_or(0, Vec::len);
        if spec_count == self.last_spec_count {
            return;
        }
        self.last_spec_count = spec_count;
        let endpoints = self.copilot_endpoints.load();
        let ep_map = endpoints.as_deref();
        let entries: Vec<ModelEntry> = guard
            .as_deref()
            .unwrap()
            .iter()
            .filter_map(|s| parse_model_entry(s, ep_map))
            .collect();
        self.picker.replace_items(entries);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
        {
            let spec = entry.spec.clone();
            let label = entry.label.clone();
            self.picker
                .with_item_mut(&label, |e| e.tier = tier.to_string());
            return ModelPickerAction::AssignTier(spec, tier);
        }
        match self.picker.handle_key(key) {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            PickerAction::Select(_, entry) => ModelPickerAction::Select(entry.spec),
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.try_refresh();
        self.picker.view(frame, area)
    }
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn parse_model_entry(
    spec: &str,
    copilot_endpoints: Option<&HashMap<String, String>>,
) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name()
    } else {
        dynamic::display_name(provider_str)?
    };

    let tier = match maki_providers::Model::from_spec(spec) {
        Ok(m) => m.tier.to_string(),
        Err(_) => String::new(),
    };

    let label = if provider_str == "copilot" {
        copilot_endpoints
            .and_then(|map| map.get(model_id))
            .map(|tag| format!("[{tag}] {model_id}"))
            .unwrap_or_else(|| model_id.to_owned())
    } else {
        model_id.to_owned()
    };

    Some(ModelEntry {
        spec: spec.to_string(),
        label,
        provider_display,
        tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn alt_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[allow(clippy::type_complexity)]
    fn test_models() -> (
        Arc<ArcSwapOption<Vec<String>>>,
        Arc<ArcSwapOption<HashMap<String, String>>>,
    ) {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        let endpoints = Arc::new(ArcSwapOption::empty());
        (models, endpoints)
    }

    #[test]
    fn select_returns_full_spec() {
        let (models, endpoints) = test_models();
        let mut p = ModelPicker::new(models, endpoints);
        p.open("");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-sonnet-4-20250514")
        );
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let (models, endpoints) = test_models();
        let mut p = ModelPicker::new(models, endpoints);
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn open_with_no_models_still_opens() {
        let models = Arc::new(ArcSwapOption::empty());
        let endpoints = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models, endpoints);
        p.open("");
        assert!(p.is_open());
    }

    #[test]
    fn refresh_populates_when_models_arrive() {
        let models = Arc::new(ArcSwapOption::empty());
        let endpoints = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone(), endpoints);
        p.open("");
        assert_eq!(p.last_spec_count, 0);

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        p.try_refresh();
        assert_eq!(p.last_spec_count, 1);
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let endpoints = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone(), endpoints);
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        assert_eq!(p.last_spec_count, 2);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let (models, endpoints) = test_models();
        let mut p = ModelPicker::new(models, endpoints);
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514", None).unwrap();
        assert_eq!(entry.label, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.tier.is_empty());
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash", None).is_none());
    }

    #[test]
    fn parse_model_entry_copilot_with_endpoint() {
        let mut map = HashMap::new();
        map.insert("gpt-5-mini".to_string(), "responses".to_string());
        let entry = parse_model_entry("copilot/gpt-5-mini", Some(&map)).unwrap();
        assert_eq!(entry.label, "[responses] gpt-5-mini");
        assert_eq!(entry.spec, "copilot/gpt-5-mini");
    }

    #[test]
    fn parse_model_entry_copilot_without_endpoint() {
        let entry = parse_model_entry("copilot/gpt-5-mini", None).unwrap();
        assert_eq!(entry.label, "gpt-5-mini");
    }

    // Regression: Alt+1/2/3 must work on every provider, not just Ollama.
    #[test_case(KeyCode::Char('1'), ModelTier::Strong ; "alt_1_strong")]
    #[test_case(KeyCode::Char('2'), ModelTier::Medium ; "alt_2_medium")]
    #[test_case(KeyCode::Char('3'), ModelTier::Weak   ; "alt_3_weak")]
    fn tier_shortcut_assigns_and_keeps_picker_open(code: KeyCode, want: ModelTier) {
        let (models, endpoints) = test_models();
        let mut p = ModelPicker::new(models, endpoints);
        p.open("");
        let action = p.handle_key(alt_key(code));
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(p.is_open());
    }

    #[test]
    fn plain_number_keys_go_to_filter() {
        let (models, endpoints) = test_models();
        let mut p = ModelPicker::new(models, endpoints);
        p.open("");
        let action = p.handle_key(key(KeyCode::Char('1')));
        assert!(
            matches!(action, ModelPickerAction::Consumed),
            "plain '1' should be consumed by filter, not trigger tier assignment"
        );
    }
}
