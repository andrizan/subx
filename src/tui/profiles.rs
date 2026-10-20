//! Profile editor state (pure, testable) and the TOML persistence that keeps
//! the rest of `subx.local.toml` — comments included — untouched.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, value};

use crate::common::config::{AudioSpec, Profile};
use crate::common::fs::write_atomic;

/// Editable fields of a profile, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Tag,
    Width,
    Height,
    Encoder,
    Video,
    AudioCodec,
    Bitrate,
    Color,
    Crop,
    Source,
    Script,
}

impl Field {
    pub const ALL: [Field; 12] = [
        Field::Name,
        Field::Tag,
        Field::Width,
        Field::Height,
        Field::Encoder,
        Field::Video,
        Field::AudioCodec,
        Field::Bitrate,
        Field::Color,
        Field::Crop,
        Field::Source,
        Field::Script,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Tag => "tag",
            Field::Width => "width",
            Field::Height => "height",
            Field::Encoder => "encoder",
            Field::Video => "video args",
            Field::AudioCodec => "audio codec",
            Field::Bitrate => "audio kbps",
            Field::Color => "color",
            Field::Crop => "crop",
            Field::Source => "source",
            Field::Script => "script",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Field::Name => "profile key used by --profile and [encode] profile",
            Field::Tag => "parenthesized tag in output names, empty for none",
            Field::Width | Field::Height => "0 keeps the source size",
            Field::Encoder => "←/→ to switch",
            Field::Video => "encoder arguments, copied from StaxRip's command line",
            Field::AudioCodec => "←/→ to switch",
            Field::Bitrate => "kbit/s",
            Field::Color => "←/→: bt709, bt601 or pass the source values through",
            Field::Crop => "none, auto, or left,top,right,bottom",
            Field::Source => "←/→: ffmpeg (zscale) or vapoursynth (vspipe + script)",
            Field::Script => "VapourSynth template, needed when source = vapoursynth",
        }
    }

    /// Choice fields cycle with ←/→ instead of free text.
    pub fn choices(self) -> Option<&'static [&'static str]> {
        match self {
            Field::Encoder => Some(&["svt-av1", "x265"]),
            Field::AudioCodec => Some(&["opus", "aac-lc", "he-aac", "he-aac-v2", "xhe-aac"]),
            Field::Color => Some(&["bt709", "bt601", "source"]),
            Field::Source => Some(&["ffmpeg", "vapoursynth"]),
            _ => None,
        }
    }
}

/// A profile being edited: the name plus the profile, with one field's text
/// buffer while typing.
#[derive(Debug, Clone)]
pub struct Form {
    pub name: String,
    pub profile: Profile,
    pub field: usize,
    /// Text being typed for the selected field; `None` when not editing.
    pub editing: Option<String>,
    pub dirty: bool,
}

impl Form {
    pub fn new(name: &str, profile: Profile) -> Self {
        Self {
            name: name.to_string(),
            profile,
            field: 0,
            editing: None,
            dirty: false,
        }
    }

    pub fn blank() -> Self {
        Self::new("new-profile", Profile::default())
    }

    pub fn selected(&self) -> Field {
        Field::ALL[self.field]
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % Field::ALL.len();
    }

    pub fn prev_field(&mut self) {
        self.field = (self.field + Field::ALL.len() - 1) % Field::ALL.len();
    }

    /// Current display value of a field.
    pub fn value(&self, field: Field) -> String {
        let p = &self.profile;
        match field {
            Field::Name => self.name.clone(),
            Field::Tag => p.tag.clone().unwrap_or_default(),
            Field::Width => p.width.to_string(),
            Field::Height => p.height.to_string(),
            Field::Encoder => p.encoder.clone(),
            Field::Video => p.video.join(" "),
            Field::AudioCodec => p.audio.codec.clone(),
            Field::Bitrate => p.audio.bitrate.to_string(),
            Field::Color => p.color.clone(),
            Field::Crop => p.crop.clone(),
            Field::Source => p.source.clone(),
            Field::Script => p
                .script
                .as_ref()
                .map(|s| s.display().to_string())
                .unwrap_or_default(),
        }
    }

    /// Cycle a choice field (`step` = ±1); no-op for text fields.
    pub fn cycle(&mut self, step: i32) {
        let field = self.selected();
        let Some(choices) = field.choices() else {
            return;
        };
        let current = self.value(field);
        let idx = choices.iter().position(|c| *c == current).unwrap_or(0) as i32;
        let next = (idx + step).rem_euclid(choices.len() as i32) as usize;
        self.set(field, choices[next]).ok();
        self.dirty = true;
    }

    /// Start typing into the selected text field (choice fields ignore this).
    pub fn begin_edit(&mut self) {
        let field = self.selected();
        if field.choices().is_none() {
            self.editing = Some(self.value(field));
        }
    }

    pub fn type_char(&mut self, c: char) {
        if let Some(buf) = &mut self.editing {
            buf.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(buf) = &mut self.editing {
            buf.pop();
        }
    }

    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Apply the typed text; returns the validation error, if any (the buffer
    /// stays open so it can be corrected).
    pub fn commit_edit(&mut self) -> Result<()> {
        let Some(buf) = self.editing.clone() else {
            return Ok(());
        };
        let field = self.selected();
        self.set(field, buf.trim())?;
        self.editing = None;
        self.dirty = true;
        Ok(())
    }

    /// Set a field from text with validation.
    pub fn set(&mut self, field: Field, text: &str) -> Result<()> {
        let p = &mut self.profile;
        match field {
            Field::Name => {
                if text.is_empty()
                    || !text
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    bail!("name: letters, digits, '-', '_' and '.' only");
                }
                self.name = text.to_string();
            }
            Field::Tag => p.tag = (!text.is_empty()).then(|| text.to_string()),
            Field::Width => p.width = text.parse().context("width must be a number")?,
            Field::Height => p.height = text.parse().context("height must be a number")?,
            Field::Encoder => p.encoder = text.to_string(),
            Field::Video => p.video = text.split_whitespace().map(str::to_string).collect(),
            Field::AudioCodec => p.audio.codec = text.to_string(),
            Field::Bitrate => p.audio.bitrate = text.parse().context("bitrate must be a number")?,
            Field::Color => p.color = text.to_string(),
            Field::Crop => {
                validate_crop(text)?;
                p.crop = text.to_string();
            }
            Field::Source => p.source = text.to_string(),
            Field::Script => p.script = (!text.is_empty()).then(|| text.into()),
        }
        Ok(())
    }

    /// Whole-profile checks before saving.
    pub fn validate(&self) -> Result<()> {
        let p = &self.profile;
        if (p.width == 0) != (p.height == 0) {
            bail!("width and height must both be set or both be 0");
        }
        if p.source == "vapoursynth" && p.script.is_none() {
            bail!("source = vapoursynth needs a script");
        }
        if p.audio.bitrate == 0 {
            bail!("audio kbps must be greater than 0");
        }
        validate_crop(&p.crop)
    }
}

fn validate_crop(text: &str) -> Result<()> {
    let t = text.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("auto") {
        return Ok(());
    }
    let parts: Vec<&str> = t.split(',').collect();
    if parts.len() != 4 || parts.iter().any(|n| n.trim().parse::<u32>().is_err()) {
        bail!("crop must be none, auto or left,top,right,bottom");
    }
    Ok(())
}

/// Parse (or start) the local config document.
fn load_doc(path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    text.parse::<DocumentMut>()
        .with_context(|| format!("{} is not valid TOML", path.display()))
}

fn profiles_table(doc: &mut DocumentMut) -> &mut toml_edit::Table {
    let encode = doc
        .entry("encode")
        .or_insert(Item::Table(toml_edit::Table::new()));
    if let Some(t) = encode.as_table_mut() {
        // `[encode]` may hold only sub-tables; keep it out of the file then.
        t.set_implicit(true);
    }
    let profiles = encode
        .as_table_mut()
        .expect("encode is a table")
        .entry("profiles")
        .or_insert(Item::Table(toml_edit::Table::new()));
    let table = profiles.as_table_mut().expect("profiles is a table");
    table.set_implicit(true);
    table
}

/// Write `[encode.profiles.<name>]` into the local config, replacing an
/// existing entry of that name and leaving everything else as it was.
/// `make_default` also sets `[encode] profile = name`.
pub fn save_profile(path: &Path, name: &str, profile: &Profile, make_default: bool) -> Result<()> {
    let mut doc = load_doc(path)?;
    let serialized =
        toml_edit::ser::to_document(profile).context("cannot serialize the profile")?;
    let mut table = serialized.as_table().clone();
    // `audio = { codec = "...", bitrate = N }` reads better than a sub-table.
    if let Some(audio) = table.remove("audio").and_then(|a| a.into_table().ok()) {
        table.insert("audio", value(audio.into_inline_table()));
    }
    table.set_implicit(false);
    profiles_table(&mut doc).insert(name, Item::Table(table));
    if make_default {
        doc["encode"]["profile"] = value(name);
        if let Some(t) = doc["encode"].as_table_mut() {
            t.set_implicit(false);
        }
    }
    write_atomic(path, &doc.to_string())
}

/// Remove `[encode.profiles.<name>]`; `Ok(false)` when it was not there.
pub fn delete_profile(path: &Path, name: &str) -> Result<bool> {
    let mut doc = load_doc(path)?;
    let removed = doc
        .get_mut("encode")
        .and_then(Item::as_table_mut)
        .and_then(|e| e.get_mut("profiles"))
        .and_then(Item::as_table_mut)
        .map(|p| p.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        write_atomic(path, &doc.to_string())?;
    }
    Ok(removed)
}

/// Set `[encode] profile = name` without touching anything else.
pub fn set_default_profile(path: &Path, name: &str) -> Result<()> {
    let mut doc = load_doc(path)?;
    doc["encode"]["profile"] = value(name);
    if let Some(t) = doc["encode"].as_table_mut() {
        t.set_implicit(false);
    }
    write_atomic(path, &doc.to_string())
}

/// The audio part as a form-friendly summary.
pub fn audio_summary(a: &AudioSpec) -> String {
    format!("{} {} kbps", a.codec, a.bitrate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::AppConfig;

    #[test]
    fn form_edits_and_validates() {
        let mut f = Form::blank();
        assert_eq!(f.selected(), Field::Name);
        // Editing starts from the current value so it can be corrected in place.
        f.begin_edit();
        assert_eq!(f.editing.as_deref(), Some("new-profile"));
        for _ in 0.."new-profile".len() {
            f.backspace();
        }
        for c in "anime480p".chars() {
            f.type_char(c);
        }
        f.commit_edit().unwrap();
        assert_eq!(f.name, "anime480p");
        assert!(f.dirty);

        // width/height must be numbers and consistent.
        f.field = Field::ALL.iter().position(|x| *x == Field::Width).unwrap();
        f.begin_edit();
        f.backspace();
        f.type_char('8');
        f.type_char('x');
        assert!(f.commit_edit().is_err());
        assert!(f.editing.is_some(), "buffer stays open to fix the typo");
        f.backspace();
        f.type_char('4');
        f.type_char('8');
        f.commit_edit().unwrap();
        assert_eq!(f.profile.width, 848);
        assert!(f.validate().is_err(), "height still 0");
        f.set(Field::Height, "480").unwrap();
        f.validate().unwrap();

        // Choice fields cycle and wrap.
        f.field = Field::ALL
            .iter()
            .position(|x| *x == Field::Encoder)
            .unwrap();
        f.cycle(1);
        assert_eq!(f.profile.encoder, "x265");
        f.cycle(1);
        assert_eq!(f.profile.encoder, "svt-av1");
        f.cycle(-1);
        assert_eq!(f.profile.encoder, "x265");
        f.begin_edit();
        assert!(f.editing.is_none(), "choice fields are not typed into");

        f.set(Field::Video, " --preset 4  --crf 35 ").unwrap();
        assert_eq!(f.profile.video, ["--preset", "4", "--crf", "35"]);
        assert!(f.set(Field::Crop, "1,2").is_err());
        f.set(Field::Crop, "0,138,0,138").unwrap();
        f.set(Field::Source, "vapoursynth").unwrap();
        assert!(f.validate().is_err(), "vapoursynth needs a script");
        f.set(Field::Script, "x.vpy").unwrap();
        f.validate().unwrap();
        assert!(f.set(Field::Name, "bad name!").is_err());
        assert_eq!(f.value(Field::Script), "x.vpy");
        assert_eq!(audio_summary(&f.profile.audio), "opus 96 kbps");
    }

    #[test]
    fn saves_profiles_without_disturbing_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subx.local.toml");
        std::fs::write(
            &path,
            "# machine notes\nworkers = 4\n\n[tools]\nstaxrip_dir = \"D:/S\"   # bundle\n\n[encode]\nprefix = \"[P]\"\n",
        )
        .unwrap();

        let mut profile = Profile {
            tag: Some("BD 480p".into()),
            width: 848,
            height: 480,
            video: vec!["--preset".into(), "4".into()],
            ..Profile::default()
        };
        save_profile(&path, "a480", &profile, true).unwrap();
        profile.encoder = "x265".into();
        save_profile(&path, "hevc", &profile, false).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# machine notes") && text.contains("# bundle"),
            "{text}"
        );
        assert!(text.contains("[encode.profiles.a480]"), "{text}");
        assert!(
            text.contains("audio = { codec = \"opus\", bitrate = 96 }"),
            "{text}"
        );
        assert!(text.contains("profile = \"a480\""), "{text}");
        assert!(!text.contains("script"), "None fields stay out: {text}");

        let cfg = AppConfig::load(Some(&path));
        assert_eq!(cfg.workers, 4);
        assert_eq!(cfg.encode.prefix, "[P]");
        assert_eq!(cfg.encode.profile.as_deref(), Some("a480"));
        assert_eq!(cfg.encode.profiles["a480"].video, ["--preset", "4"]);
        assert_eq!(cfg.encode.profiles["hevc"].encoder, "x265");
        assert_eq!(cfg.tools.staxrip_dir.as_deref(), Some(Path::new("D:/S")));

        // Re-saving replaces, deleting removes, default switches.
        profile.width = 1280;
        profile.height = 720;
        save_profile(&path, "a480", &profile, false).unwrap();
        let cfg = AppConfig::load(Some(&path));
        assert_eq!(cfg.encode.profiles["a480"].width, 1280);
        assert_eq!(cfg.encode.profiles.len(), 2);
        assert!(delete_profile(&path, "hevc").unwrap());
        assert!(!delete_profile(&path, "hevc").unwrap());
        set_default_profile(&path, "a480").unwrap();
        let cfg = AppConfig::load(Some(&path));
        assert_eq!(cfg.encode.profiles.len(), 1);
        assert_eq!(cfg.encode.profile.as_deref(), Some("a480"));
    }

    #[test]
    fn saving_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.toml");
        save_profile(&path, "p", &Profile::default(), true).unwrap();
        let cfg = AppConfig::load(Some(&path));
        assert_eq!(cfg.encode.profile.as_deref(), Some("p"));
        assert!(cfg.encode.profiles.contains_key("p"));
    }
}
