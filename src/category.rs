use ratatui::style::Color;
use rustc_hash::FxHashMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// Index into the category registry.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Category(u8);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconMode {
    #[default]
    Emoji,
    /// Nerd Font glyphs (needs a patched font)
    Nerd,
    None,
}

pub struct CategoryDef {
    pub name: String,
    pub color: Color,
    pub icon: Option<String>,
    pub nerd_icon: Option<String>,
    /// Has directory rules: its popup lists matching directories, not files.
    pub dir_kind: bool,
    pub weak: bool,
}

#[derive(Default)]
pub struct Registry {
    pub defs: Vec<CategoryDef>,
    ext: FxHashMap<String, Category>,
    /// Final normalized rules exceeding the stack-buffer fast path, sorted by
    /// extension. Keep `ext` authoritative for rule ownership queries.
    long_ext: Vec<(String, Category)>,
    dirs: FxHashMap<String, Category>,
    /// name -> [(parent, category)]
    parent_dirs: FxHashMap<String, Vec<(String, Category)>>,
    markers: FxHashMap<String, MarkerRule>,
    /// Bit `n` set when some marker is `n` bytes long: a cheap pre-filter, as
    /// this check runs for every directory entry.
    marker_lens: u64,
    other: Category,
    pub icons: IconMode,
    /// Hash of the rules and the diskbytype version; a cached scan made with a
    /// different fingerprint has stale categories.
    pub fingerprint: u64,
}

#[derive(Default)]
struct MarkerRule {
    /// Matches in any directory.
    any: Option<Category>,
    /// (dir name, category): only matches in directories with that name.
    named: Vec<(String, Category)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    icons: Option<IconMode>,
    #[serde(default)]
    category: Vec<CategoryConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryConfig {
    name: String,
    color: Option<ColorConfig>,
    icon: Option<String>,
    nerd_icon: Option<String>,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    dirs: Vec<String>,
    #[serde(default)]
    markers: Vec<String>,
    weak: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ColorConfig {
    Indexed(u8),
    Hex(String),
}

impl ColorConfig {
    fn to_color(&self) -> Result<Color, String> {
        match self {
            ColorConfig::Indexed(i) => Ok(Color::Indexed(*i)),
            ColorConfig::Hex(s) => {
                let h = s.strip_prefix('#').unwrap_or(s);
                let v = u32::from_str_radix(h, 16).ok().filter(|_| h.len() == 6);
                let v = v.ok_or_else(|| format!("invalid color {s:?}, expected 0-255 or \"#rrggbb\""))?;
                Ok(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
            }
        }
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// The active registry. Falls back to the built-in rules if `init` was not called.
pub fn reg() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry::from_sources(&[DEFAULT_CONFIG]).expect("built-in config is valid"))
}

/// Load built-in rules plus the user config (explicit path, or the default
/// location if present). Only a missing implicit config is ignored; other read
/// errors and all explicit-path errors are reported. Returns the applied path.
pub fn init(explicit: Option<&Path>, icons: Option<IconMode>) -> Result<Option<PathBuf>, String> {
    let mut path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => default_config_path(),
    };
    let user = match &path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(user) => Some(user),
            Err(e) if explicit.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
                path = None;
                None
            }
            Err(e) => return Err(format!("{}: {e}", p.display())),
        },
        None => None,
    };
    let mut sources = vec![DEFAULT_CONFIG];
    sources.extend(user.as_deref());
    let mut registry = Registry::from_sources(&sources)
        .map_err(|e| format!("{}: {e}", path.as_deref().unwrap_or(Path::new("config")).display()))?;
    if let Some(mode) = icons {
        registry.icons = mode;
    }
    let _ = REGISTRY.set(registry);
    Ok(path)
}

pub fn default_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DISKBYTYPE_CONFIG").or_else(|| std::env::var_os("DUTYPE_CONFIG")) {
        return Some(PathBuf::from(p));
    }
    let new = config_path_for("diskbytype")?;
    if !new.exists()
        && let Some(old) = config_path_for("dutype").filter(|p| p.exists())
    {
        // Pre-rename config, read until the user moves it.
        return Some(old);
    }
    Some(new)
}

fn config_path_for(app: &str) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join(app).join("config.toml"))
}

impl Registry {
    fn from_sources(sources: &[&str]) -> Result<Self, String> {
        let mut r = Registry {
            fingerprint: crate::cache::fnv1a(0, env!("CARGO_PKG_VERSION").as_bytes()),
            ..Default::default()
        };
        for src in sources {
            r.fingerprint = crate::cache::fnv1a(r.fingerprint, src.as_bytes());
            let cfg: ConfigFile = toml::from_str(src).map_err(|e| e.to_string())?;
            if let Some(mode) = cfg.icons {
                r.icons = mode;
            }
            for c in cfg.category {
                r.apply(c)?;
            }
        }
        r.other = r.find("Other").unwrap_or_else(|| {
            r.defs.push(CategoryDef {
                name: "Other".into(),
                color: Color::Indexed(245),
                icon: None,
                nerd_icon: None,
                dir_kind: false,
                weak: false,
            });
            Category(r.defs.len() as u8 - 1)
        });
        // Derive this only after overrides and category-index shifts finish.
        r.long_ext = r
            .ext
            .iter()
            .filter(|(ext, _)| ext.len() > 16)
            .map(|(ext, &cat)| (ext.clone(), cat))
            .collect();
        r.long_ext.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if !r.long_ext.is_empty() {
            // Earlier same-version scans silently classified these rules as
            // Other. Invalidate those snapshots, not unaffected short rules.
            r.fingerprint = crate::cache::fnv1a(r.fingerprint, b"\0classification:long-extensions:v1");
        }
        Ok(r)
    }

    fn find(&self, name: &str) -> Option<Category> {
        self.defs
            .iter()
            .position(|d| d.name.eq_ignore_ascii_case(name))
            .map(|i| Category(i as u8))
    }

    fn apply(&mut self, c: CategoryConfig) -> Result<(), String> {
        let id = match self.find(&c.name) {
            Some(id) => id,
            None => {
                if self.defs.len() >= u8::MAX as usize {
                    return Err("too many categories".into());
                }
                // New categories go before "Other" so it stays last.
                let at = self.find("Other").map_or(self.defs.len(), |o| o.0 as usize);
                self.shift_from(at);
                self.defs.insert(
                    at,
                    CategoryDef {
                        name: c.name.clone(),
                        color: Color::Indexed(250),
                        icon: None,
                        nerd_icon: None,
                        dir_kind: false,
                        weak: false,
                    },
                );
                Category(at as u8)
            }
        };
        let def = &mut self.defs[id.0 as usize];
        if let Some(color) = &c.color {
            def.color = color.to_color()?;
        }
        if c.icon.is_some() {
            def.icon = c.icon;
        }
        if c.nerd_icon.is_some() {
            def.nerd_icon = c.nerd_icon;
        }
        if let Some(w) = c.weak {
            def.weak = w;
        }
        def.dir_kind |= !c.dirs.is_empty() || !c.markers.is_empty();
        for e in c.extensions {
            self.ext.insert(e.trim_start_matches('.').to_ascii_lowercase(), id);
        }
        for d in c.dirs {
            match d.split_once('/') {
                Some((parent, name)) => upsert(self.parent_dirs.entry(name.into()).or_default(), parent, id),
                None => {
                    self.dirs.insert(d, id);
                }
            }
        }
        for m in c.markers {
            let (dir, marker) = match m.split_once('/') {
                Some((dir, marker)) => (Some(dir), marker),
                None => (None, m.as_str()),
            };
            if marker.len() >= 64 {
                return Err(format!("marker {marker:?} is too long"));
            }
            self.marker_lens |= 1 << marker.len();
            let rule = self.markers.entry(marker.into()).or_default();
            match dir {
                Some(dir) => upsert(&mut rule.named, dir, id),
                None => rule.any = Some(id),
            }
        }
        Ok(())
    }

    /// Renumber every rule pointing at index >= `at` (an insert is about to happen).
    fn shift_from(&mut self, at: usize) {
        let bump = |c: &mut Category| {
            if c.0 as usize >= at {
                c.0 += 1;
            }
        };
        self.ext.values_mut().for_each(bump);
        self.dirs.values_mut().for_each(bump);
        for list in self.parent_dirs.values_mut() {
            list.iter_mut().for_each(|(_, c)| bump(c));
        }
        for rule in self.markers.values_mut() {
            rule.any.iter_mut().for_each(bump);
            rule.named.iter_mut().for_each(|(_, c)| bump(c));
        }
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn all(&self) -> impl Iterator<Item = Category> + '_ {
        (0..self.defs.len()).map(|i| Category(i as u8))
    }

    pub fn find_by_name(&self, name: &str) -> Option<Category> {
        self.find(name)
    }

    pub fn other(&self) -> Category {
        self.other
    }

    /// Whether the final rules can classify a filename as this category.
    /// Query setup only: do not add this map walk to per-file classification.
    pub fn has_file_rules(&self, cat: Category) -> bool {
        cat == self.other || self.ext.values().any(|&owner| owner == cat)
    }

    pub fn classify_file(&self, name: &str) -> Category {
        let Some(ext) = ext_of(name) else { return self.other };
        // Lowercase into a stack buffer: this runs once per file.
        let mut buf = [0u8; 16];
        let Some(dst) = buf.get_mut(..ext.len()) else {
            return self
                .long_ext
                .iter()
                .find(|(rule, _)| rule.eq_ignore_ascii_case(ext))
                .map_or(self.other, |(_, cat)| *cat);
        };
        dst.copy_from_slice(ext.as_bytes());
        dst.make_ascii_lowercase();
        std::str::from_utf8(dst)
            .ok()
            .and_then(|e| self.ext.get(e))
            .copied()
            .unwrap_or(self.other)
    }

    /// Classify a directory from its name, its parent's name and its entries.
    pub fn special_dir<'a>(
        &self,
        name: &str,
        parent: &str,
        entries: impl Iterator<Item = &'a str>,
    ) -> Option<Category> {
        if let Some(c) = self.parent_dirs.get(name).and_then(|l| lookup(l, parent)) {
            return Some(c);
        }
        if let Some(&c) = self.dirs.get(name) {
            return Some(c);
        }
        let mut plain = None;
        for e in entries {
            if e.len() >= 64 || self.marker_lens & (1 << e.len()) == 0 {
                continue;
            }
            if let Some(rule) = self.markers.get(e) {
                if let Some(c) = lookup(&rule.named, name) {
                    return Some(c);
                }
                plain = plain.or(rule.any);
            }
        }
        plain
    }
}

fn upsert(list: &mut Vec<(String, Category)>, key: &str, id: Category) {
    match list.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = id,
        None => list.push((key.to_string(), id)),
    }
}

fn lookup(list: &[(String, Category)], key: &str) -> Option<Category> {
    list.iter().find(|(k, _)| k == key).map(|(_, c)| *c)
}

impl Category {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The category at `index`, if the active registry has one.
    pub fn from_index(index: usize) -> Option<Category> {
        (index < reg().len()).then_some(Category(index as u8))
    }

    fn def(self) -> &'static CategoryDef {
        &reg().defs[self.0 as usize]
    }

    pub fn label(self) -> &'static str {
        &self.def().name
    }

    pub fn color(self) -> Color {
        self.def().color
    }

    pub fn is_dir_kind(self) -> bool {
        self.def().dir_kind
    }

    /// Icon for the active icon mode, if any.
    pub fn icon(self) -> Option<&'static str> {
        match reg().icons {
            IconMode::Emoji => Some(self.def().icon.as_deref().unwrap_or("📄")),
            IconMode::Nerd => Some(self.def().nerd_icon.as_deref().unwrap_or("\u{f016}")),
            IconMode::None => None,
        }
    }

    pub fn is_weak(self) -> bool {
        self.def().weak
    }
}

/// Categories with a non-zero size, biggest first.
pub fn by_size(cats: &[u64]) -> Vec<Category> {
    let mut v: Vec<Category> = reg().all().filter(|c| cats[c.index()] > 0).collect();
    v.sort_by(|a, b| cats[b.index()].cmp(&cats[a.index()]));
    v
}

pub fn dominant(cats: &[u64]) -> Category {
    reg().all().max_by_key(|c| cats[c.index()]).unwrap_or(reg().other())
}

/// Icon for a plain (non-special) directory in the active icon mode.
pub fn folder_icon() -> Option<&'static str> {
    match reg().icons {
        IconMode::Emoji => Some("📁"),
        IconMode::Nerd => Some("\u{f07b}"),
        IconMode::None => None,
    }
}

pub fn ext_of(name: &str) -> Option<&str> {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => Some(ext),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Registry {
        Registry::from_sources(&[DEFAULT_CONFIG]).unwrap()
    }

    fn label(r: &Registry, c: Option<Category>) -> Option<&str> {
        c.map(|c| r.defs[c.index()].name.as_str())
    }

    #[test]
    fn file_rule_eligibility_uses_final_owners_and_other_fallback() {
        let base = r#"
            [[category]]
            name = "Mixed"
            dirs = ["special"]
            extensions = ["one", "two"]
            [[category]]
            name = "Other"
            dirs = ["fallback"]
        "#;
        let override_one = r#"[[category]]
            name = "New owner"
            extensions = ["one"]
        "#;
        let override_two = r#"[[category]]
            name = "New owner"
            extensions = ["TWO"]
        "#;
        let partial = Registry::from_sources(&[base, override_one]).unwrap();
        assert!(partial.has_file_rules(partial.find("Mixed").unwrap()));
        let final_rules = Registry::from_sources(&[base, override_one, override_two]).unwrap();
        let mixed = final_rules.find("Mixed").unwrap();
        assert!(final_rules.defs[mixed.index()].dir_kind);
        assert!(!final_rules.has_file_rules(mixed));
        assert!(final_rules.has_file_rules(final_rules.find("New owner").unwrap()));
        assert!(final_rules.has_file_rules(final_rules.other()));
        assert_eq!(final_rules.other().index(), final_rules.len() - 1);
    }

    #[test]
    fn extensions() {
        let r = builtin();
        let l = |n| r.defs[r.classify_file(n).index()].name.as_str();
        assert_eq!(l("a.PDF"), "PDF");
        assert_eq!(l("movie.mkv"), "Video");
        assert_eq!(l("x.tar.gz"), "Archive");
        assert_eq!(l("model.safetensors"), "ML model");
        assert_eq!(l(".bashrc"), "Other");
        assert_eq!(l("Makefile"), "Other");
        assert_eq!(l("a.averyveryverylongextension"), "Other");
    }

    #[test]
    fn custom_extensions_cross_stack_and_utf8_boundaries() {
        let config = r#"
            [[category]]
            name = "Custom"
            extensions = ["abcdefghijklmno", "abcdefghijklmnop", "...ABCDEFGHIJKLMNOPQ",
                          "éééééééé", "ééééééééx"]
        "#;
        let r = Registry::from_sources(&[DEFAULT_CONFIG, config]).unwrap();
        let custom = r.find("Custom").unwrap();
        for ext in [
            "abcdefghijklmno",
            "abcdefghijklmnop",
            "abcdefghijklmnopq",
            "éééééééé",
            "ééééééééx",
        ] {
            assert_eq!(r.classify_file(&format!("file.{ext}")), custom);
            assert_eq!(r.classify_file(&format!("file.{}", ext.to_ascii_uppercase())), custom);
        }
        assert_eq!(r.classify_file("file.Éééééééé"), r.other());
        assert_eq!(r.classify_file("file.Ééééééééx"), r.other());
        assert_eq!(r.classify_file("file.abcdefghijklmnopqr"), r.other());
        assert_eq!(r.classify_file("file.abcdefghijklmnoqq"), r.other());
        assert_eq!(
            r.long_ext.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
            ["abcdefghijklmnopq", "ééééééééx"]
        );
    }

    #[test]
    fn long_rules_preserve_extension_extraction() {
        let r = Registry::from_sources(&[r#"[[category]]
            name = "Custom"
            extensions = ["abcdefghijklmnopq", "part.abcdefghijklmnopq"]
        "#])
        .unwrap();
        let custom = r.find("Custom").unwrap();
        for name in [
            "",
            ".",
            "..",
            "plain",
            ".abcdefghijklmnopq",
            "file.",
            "file.abcdefghijklmnopq.",
        ] {
            assert_eq!(r.classify_file(name), r.other(), "{name}");
        }
        for name in [
            "file.abcdefghijklmnopq",
            ".hidden.abcdefghijklmnopq",
            "file.part.abcdefghijklmnopq",
            "..abcdefghijklmnopq",
        ] {
            assert_eq!(r.classify_file(name), custom, "{name}");
        }
        assert_eq!(r.classify_file("file.abcdefghijklmnopq.short"), r.other());
    }

    #[test]
    fn long_rules_use_final_owners_and_shifted_other() {
        let base = r#"
            [[category]]
            name = "Mixed"
            dirs = ["special"]
            extensions = ["zzzzzzzzzzzzzzzzz", "abcdefghijklmnopq"]
            [[category]]
            name = "Other"
            extensions = ["otherotherotherxxx"]
        "#;
        let overrides = r#"
            [[category]]
            name = "New owner"
            extensions = [".ABCDEFGHIJKLMNOPQ", "zzzzzzzzzzzzzzzzz"]
            [[category]]
            name = "Later"
            extensions = ["short"]
        "#;
        let partial = Registry::from_sources(&[base]).unwrap();
        assert!(partial.has_file_rules(partial.find("Mixed").unwrap()));
        let r = Registry::from_sources(&[base, overrides]).unwrap();
        let new = r.find("New owner").unwrap();
        assert_eq!(r.classify_file("file.abcdefghijklmnopq"), new);
        assert_eq!(r.classify_file("file.zzzzzzzzzzzzzzzzz"), new);
        assert_eq!(r.classify_file("file.otherotherotherxxx"), r.other());
        assert!(!r.has_file_rules(r.find("Mixed").unwrap()));
        assert!(r.has_file_rules(new));
        assert!(r.has_file_rules(r.other()));
        assert_eq!(r.other().index(), r.len() - 1);
        assert_eq!(
            r.long_ext,
            vec![
                ("abcdefghijklmnopq".into(), new),
                ("otherotherotherxxx".into(), r.other()),
                ("zzzzzzzzzzzzzzzzz".into(), new)
            ]
        );
        assert_eq!(r.long_ext, Registry::from_sources(&[base, overrides]).unwrap().long_ext);
    }

    #[test]
    fn fingerprint_revision_is_selective_and_deterministic() {
        fn legacy(sources: &[&str]) -> u64 {
            sources.iter().fold(
                crate::cache::fnv1a(0, env!("CARGO_PKG_VERSION").as_bytes()),
                |hash, source| crate::cache::fnv1a(hash, source.as_bytes()),
            )
        }
        for config in [
            "",
            r#"[[category]]
            name = "Short"
            extensions = ["....................PDF", "abcdefghijklmnop", "éééééééé"]
        "#,
        ] {
            let sources = [DEFAULT_CONFIG, config];
            let r = Registry::from_sources(&sources).unwrap();
            assert!(r.long_ext.is_empty());
            assert_eq!(r.fingerprint, legacy(&sources));
        }
        for name in ["Long", "Other"] {
            let config = format!("[[category]]\nname = {name:?}\nextensions = [\"abcdefghijklmnopq\"]");
            let sources = [DEFAULT_CONFIG, &config];
            let r = Registry::from_sources(&sources).unwrap();
            assert_eq!(r.long_ext.len(), 1);
            assert_ne!(r.fingerprint, legacy(&sources));
            assert_eq!(r.fingerprint, Registry::from_sources(&sources).unwrap().fingerprint);
        }
    }

    #[test]
    fn special_dirs() {
        let r = builtin();
        let s =
            |name, parent, entries: &[&'static str]| label(&r, r.special_dir(name, parent, entries.iter().copied()));
        assert_eq!(s("node_modules", "app", &[]), Some("node_modules"));
        assert_eq!(s("myenv", "x", &["bin", "pyvenv.cfg"]), Some("Python env"));
        assert_eq!(s("target", "proj", &["CACHEDIR.TAG"]), Some("Build output"));
        assert_eq!(s("other", "proj", &["CACHEDIR.TAG"]), Some("Cache"));
        assert_eq!(s("torch", ".cache", &[]), Some("ML cache"));
        assert_eq!(s("torch", "src", &[]), None);
        assert_eq!(s("src", "proj", &["main.rs"]), None);
    }

    #[test]
    fn user_config_extends_and_adds() {
        let user = r##"
            [[category]]
            name = "video"
            extensions = ["xyz"]

            [[category]]
            name = "Unity"
            color = "#ff8800"
            extensions = ["unitypackage", "pdf"]
            dirs = ["Library"]
        "##;
        let r = Registry::from_sources(&[DEFAULT_CONFIG, user]).unwrap();
        let l = |n| r.defs[r.classify_file(n).index()].name.as_str();
        assert_eq!(l("clip.xyz"), "Video");
        assert_eq!(l("pack.unitypackage"), "Unity");
        assert_eq!(l("doc.pdf"), "Unity");
        assert_eq!(
            label(&r, r.special_dir("Library", "game", [].into_iter())),
            Some("Unity")
        );
        // Inserting before Other must not break existing rules.
        assert_eq!(l("a.mp4"), "Video");
        assert_eq!(l("Makefile"), "Other");
        assert_eq!(r.defs.last().unwrap().name, "Other");
        assert_eq!(
            label(&r, r.special_dir("node_modules", "x", [].into_iter())),
            Some("node_modules")
        );
    }

    #[test]
    fn bad_config_is_reported() {
        assert!(Registry::from_sources(&[DEFAULT_CONFIG, "[[category]]\nname = \"x\"\ncolour = 3"]).is_err());
        assert!(Registry::from_sources(&[DEFAULT_CONFIG, "[[category]]\nname = \"x\"\ncolor = \"red\""]).is_err());
    }
}
