use ratatui::style::Color;

/// What a file "is". Special-directory categories (node_modules, .venv, ...) win over
/// the file extension: every file below such a directory is counted toward it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Category {
    NodeModules,
    PyEnv,
    PyCache,
    Vcs,
    BuildOutput,
    Cache,
    Trash,
    Video,
    Image,
    Audio,
    Pdf,
    Document,
    Archive,
    DiskImage,
    Model,
    Data,
    Code,
    Other,
}

pub const COUNT: usize = 18;

pub const ALL: [Category; COUNT] = [
    Category::NodeModules,
    Category::PyEnv,
    Category::PyCache,
    Category::Vcs,
    Category::BuildOutput,
    Category::Cache,
    Category::Trash,
    Category::Video,
    Category::Image,
    Category::Audio,
    Category::Pdf,
    Category::Document,
    Category::Archive,
    Category::DiskImage,
    Category::Model,
    Category::Data,
    Category::Code,
    Category::Other,
];

impl Category {
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::NodeModules => "node_modules",
            Category::PyEnv => "Python env",
            Category::PyCache => "Python cache",
            Category::Vcs => ".git / VCS",
            Category::BuildOutput => "Build output",
            Category::Cache => "Cache",
            Category::Trash => "Trash",
            Category::Video => "Video",
            Category::Image => "Image",
            Category::Audio => "Audio",
            Category::Pdf => "PDF",
            Category::Document => "Document",
            Category::Archive => "Archive",
            Category::DiskImage => "Disk image",
            Category::Model => "ML model",
            Category::Data => "Data",
            Category::Code => "Code",
            Category::Other => "Other",
        }
    }

    pub fn color(self) -> Color {
        Color::Indexed(match self {
            Category::NodeModules => 203,
            Category::PyEnv => 214,
            Category::PyCache => 180,
            Category::Vcs => 166,
            Category::BuildOutput => 170,
            Category::Cache => 139,
            Category::Trash => 240,
            Category::Video => 39,
            Category::Image => 42,
            Category::Audio => 51,
            Category::Pdf => 227,
            Category::Document => 229,
            Category::Archive => 141,
            Category::DiskImage => 99,
            Category::Model => 201,
            Category::Data => 117,
            Category::Code => 120,
            Category::Other => 245,
        })
    }

    /// Categories that come from a directory rather than a file extension.
    pub fn is_dir_kind(self) -> bool {
        self.index() <= Category::Trash.index()
    }
}

pub fn ext_of(name: &str) -> Option<&str> {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => Some(ext),
        _ => None,
    }
}

pub fn from_name(name: &str) -> Category {
    let Some(ext) = ext_of(name) else {
        return Category::Other;
    };
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "mov" | "avi" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "3gp"
        | "mts" | "m2ts" => Category::Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif"
        | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "svg" | "psd" | "ico" | "avif" => {
            Category::Image
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "aif" | "aiff" | "opus" => {
            Category::Audio
        }
        "pdf" => Category::Pdf,
        "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "key"
        | "pages" | "numbers" | "txt" | "md" | "rtf" | "epub" | "tex" => Category::Document,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "tbz"
        | "txz" | "whl" | "jar" => Category::Archive,
        "dmg" | "iso" | "img" | "pkg" | "vmdk" | "qcow2" | "vdi" | "vhd" | "vhdx" => {
            Category::DiskImage
        }
        "pt" | "pth" | "ckpt" | "safetensors" | "onnx" | "h5" | "pb" | "tflite" | "gguf"
        | "mlmodel" | "keras" => Category::Model,
        "csv" | "tsv" | "json" | "jsonl" | "parquet" | "npy" | "npz" | "feather" | "arrow"
        | "sqlite" | "sqlite3" | "db" | "hdf5" | "pkl" | "pickle" | "mat" | "tfrecord"
        | "mdb" => Category::Data,
        "rs" | "py" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cc"
        | "cpp" | "hpp" | "java" | "kt" | "swift" | "rb" | "php" | "sh" | "zsh" | "html"
        | "css" | "scss" | "vue" | "svelte" | "lua" | "zig" | "cs" | "m" | "mm" | "sql"
        | "toml" | "yaml" | "yml" | "ipynb" => Category::Code,
        _ => Category::Other,
    }
}

/// Decide whether a directory is a "special" one, using its name and the names of
/// its direct entries (so no extra syscalls are needed).
pub fn special_dir<'a>(name: &str, entries: impl Iterator<Item = &'a str>) -> Option<Category> {
    match name {
        "node_modules" => return Some(Category::NodeModules),
        "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".tox"
        | ".ipynb_checkpoints" => return Some(Category::PyCache),
        ".git" | ".hg" | ".svn" => return Some(Category::Vcs),
        ".Trash" | ".Trashes" => return Some(Category::Trash),
        ".cache" | "Caches" | ".npm" | ".pnpm-store" | ".gradle" => return Some(Category::Cache),
        ".next" | ".nuxt" | ".turbo" | ".parcel-cache" | ".svelte-kit" | ".angular" => {
            return Some(Category::BuildOutput);
        }
        _ => {}
    }
    let mut cachedir_tag = false;
    for e in entries {
        match e {
            "pyvenv.cfg" | "conda-meta" => return Some(Category::PyEnv),
            "CACHEDIR.TAG" => cachedir_tag = true,
            _ => {}
        }
    }
    // Cargo writes CACHEDIR.TAG into target/; other tools use it for caches.
    cachedir_tag.then(|| {
        if name == "target" {
            Category::BuildOutput
        } else {
            Category::Cache
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(from_name("a.PDF"), Category::Pdf);
        assert_eq!(from_name("movie.mkv"), Category::Video);
        assert_eq!(from_name("x.tar.gz"), Category::Archive);
        assert_eq!(from_name(".bashrc"), Category::Other);
        assert_eq!(from_name("Makefile"), Category::Other);
    }

    #[test]
    fn special_dirs() {
        assert_eq!(special_dir("node_modules", [].into_iter()), Some(Category::NodeModules));
        assert_eq!(special_dir("myenv", ["bin", "pyvenv.cfg"].into_iter()), Some(Category::PyEnv));
        assert_eq!(special_dir("target", ["CACHEDIR.TAG"].into_iter()), Some(Category::BuildOutput));
        assert_eq!(special_dir("src", ["main.rs"].into_iter()), None);
    }

    #[test]
    fn all_is_ordered() {
        for (i, c) in ALL.iter().enumerate() {
            assert_eq!(c.index(), i);
        }
    }
}
