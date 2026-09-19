use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const LIBRARY_VERSION: u8 = 1;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub local_playback: bool,
    #[serde(default)]
    pub audio: AudioSettings,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct AudioSettings {
    #[serde(default)]
    pub onboarding_complete: bool,
    pub mix_sink: Option<String>,
    pub virtual_source: Option<String>,
    pub physical_source: Option<String>,
    #[serde(default)]
    pub managed_mix: bool,
}

pub struct Sound {
    pub name: String,
    pub path: PathBuf,
    pub aliases: Vec<String>,
    pub volume: u8,
}

pub struct ImportReport {
    pub imported: usize,
    pub errors: Vec<String>,
}

impl Sound {
    fn from_path(path: PathBuf, metadata: Option<SoundMetadata>) -> Self {
        let metadata = metadata.unwrap_or_else(|| SoundMetadata {
            file: file_name(&path),
            name: default_name(&path),
            aliases: Vec::new(),
            volume: default_volume(),
        });

        Self {
            name: metadata.name,
            path,
            aliases: metadata.aliases,
            volume: metadata.volume.min(100),
        }
    }

    pub fn file_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
    }

    fn metadata(&self) -> SoundMetadata {
        SoundMetadata {
            file: self.file_name().to_string(),
            name: self.name.clone(),
            aliases: self.aliases.clone(),
            volume: self.volume,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct LibraryFile {
    #[serde(default = "library_version")]
    version: u8,
    #[serde(default)]
    sounds: Vec<SoundMetadata>,
}

#[derive(Deserialize, Serialize)]
struct SoundMetadata {
    file: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default = "default_volume")]
    volume: u8,
}

pub fn load_sounds() -> io::Result<Vec<Sound>> {
    load_sounds_from(&data_dir()?)
}

pub fn save_sounds(sounds: &[Sound]) -> io::Result<()> {
    save_library_file(&data_dir()?.join("library.toml"), sounds)
}

pub fn import_sounds(paths: &[PathBuf]) -> io::Result<ImportReport> {
    import_sounds_into(&data_dir()?, paths)
}

pub fn delete_sound(path: &Path) -> io::Result<()> {
    delete_sound_from(&data_dir()?, path)
}

pub fn load_settings() -> io::Result<Settings> {
    load_settings_from(&data_dir()?.join("settings.toml"))
}

fn load_settings_from(path: &Path) -> io::Result<Settings> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(error),
    }
}

pub fn save_settings(settings: &Settings) -> io::Result<()> {
    save_settings_to(&data_dir()?.join("settings.toml"), settings)
}

fn save_settings_to(path: &Path, settings: &Settings) -> io::Result<()> {
    let contents = toml::to_string_pretty(settings).map_err(io::Error::other)?;
    let temporary_path = path.with_extension("toml.tmp");

    fs::write(&temporary_path, contents)?;
    fs::rename(temporary_path, path)
}

fn load_sounds_from(data_dir: &Path) -> io::Result<Vec<Sound>> {
    let sounds_dir = data_dir.join("sounds");
    let library_path = data_dir.join("library.toml");
    fs::create_dir_all(&sounds_dir)?;

    let library = load_library_file(&library_path)?;
    let mut metadata_by_file = library
        .sounds
        .into_iter()
        .map(|metadata| (metadata.file.clone(), metadata))
        .collect::<HashMap<_, _>>();

    let mut paths = fs::read_dir(&sounds_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_supported_audio(path))
        .collect::<Vec<_>>();
    paths.sort();

    let sounds = paths
        .into_iter()
        .map(|path| {
            let metadata = metadata_by_file.remove(&file_name(&path));
            Sound::from_path(path, metadata)
        })
        .collect::<Vec<_>>();

    // Reescrever sincroniza arquivos novos e remove entradas sem audio.
    save_library_file(&library_path, &sounds)?;

    Ok(sounds)
}

fn import_sounds_into(data_dir: &Path, paths: &[PathBuf]) -> io::Result<ImportReport> {
    let sounds_dir = data_dir.join("sounds");
    fs::create_dir_all(&sounds_dir)?;

    let mut report = ImportReport {
        imported: 0,
        errors: Vec::new(),
    };

    for source in paths {
        let result = (|| {
            if !source.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a regular file",
                ));
            }
            if !is_supported_audio(source) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported audio format",
                ));
            }

            let destination = available_destination(&sounds_dir, source)?;
            fs::copy(source, destination)?;
            Ok(())
        })();

        match result {
            Ok(()) => report.imported += 1,
            Err(error) => report.errors.push(format!("{}: {error}", source.display())),
        }
    }

    Ok(report)
}

fn delete_sound_from(data_dir: &Path, path: &Path) -> io::Result<()> {
    let sounds_dir = data_dir.join("sounds");
    if path.parent() != Some(sounds_dir.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "audio file is outside the managed sounds directory",
        ));
    }

    fs::remove_file(path)
}

fn available_destination(sounds_dir: &Path, source: &Path) -> io::Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
    let direct_destination = sounds_dir.join(file_name);

    if !direct_destination.exists() {
        return Ok(direct_destination);
    }

    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file name"))?;
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid file extension"))?;

    for suffix in 1.. {
        let candidate = sounds_dir.join(format!("{stem} ({suffix}).{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    unreachable!("an available file name must exist")
}

fn data_dir() -> io::Result<PathBuf> {
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "data directory not found"))?;

    Ok(data_home.join("soundboard-tui"))
}

fn load_library_file(path: &Path) -> io::Result<LibraryFile> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LibraryFile {
            version: LIBRARY_VERSION,
            sounds: Vec::new(),
        }),
        Err(error) => Err(error),
    }
}

fn save_library_file(path: &Path, sounds: &[Sound]) -> io::Result<()> {
    let library = LibraryFile {
        version: LIBRARY_VERSION,
        sounds: sounds.iter().map(Sound::metadata).collect(),
    };
    let contents = toml::to_string_pretty(&library).map_err(io::Error::other)?;
    let temporary_path = path.with_extension("toml.tmp");

    fs::write(&temporary_path, contents)?;
    fs::rename(temporary_path, path)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn default_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .replace(['-', '_'], " ")
}

pub(crate) fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_lowercase().as_str(),
                "aac" | "flac" | "m4a" | "mp3" | "ogg" | "opus" | "wav"
            )
        })
        .unwrap_or(false)
}

fn library_version() -> u8 {
    LIBRARY_VERSION
}

fn default_volume() -> u8 {
    100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_name_from_file_name() {
        assert_eq!(
            default_name(Path::new("/tmp/cartoon-hammer_sound.mp3")),
            "cartoon hammer sound"
        );
    }

    #[test]
    fn recognizes_supported_audio_extensions() {
        assert!(is_supported_audio(Path::new("sound.MP3")));
        assert!(is_supported_audio(Path::new("sound.ogg")));
        assert!(!is_supported_audio(Path::new("notes.txt")));
    }

    #[test]
    fn reconciles_files_with_persisted_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let sounds_dir = temporary.path().join("sounds");
        fs::create_dir(&sounds_dir).unwrap();
        fs::write(sounds_dir.join("bonk.mp3"), []).unwrap();
        fs::write(sounds_dir.join("new-sound.wav"), []).unwrap();
        fs::write(
            temporary.path().join("library.toml"),
            r#"
version = 1

[[sounds]]
file = "bonk.mp3"
name = "Bonk"
aliases = ["martelo"]
volume = 75

[[sounds]]
file = "missing.mp3"
name = "Missing"
aliases = []
volume = 100
"#,
        )
        .unwrap();

        let mut sounds = load_sounds_from(temporary.path()).unwrap();

        assert_eq!(sounds.len(), 2);
        assert_eq!(sounds[0].name, "Bonk");
        assert_eq!(sounds[0].aliases, ["martelo"]);
        assert_eq!(sounds[0].volume, 75);
        assert_eq!(sounds[1].name, "new sound");

        let saved = fs::read_to_string(temporary.path().join("library.toml")).unwrap();
        assert!(saved.contains("bonk.mp3"));
        assert!(saved.contains("new-sound.wav"));
        assert!(!saved.contains("missing.mp3"));

        sounds[0].volume = 40;
        save_library_file(&temporary.path().join("library.toml"), &sounds).unwrap();

        let reloaded = load_sounds_from(temporary.path()).unwrap();
        assert_eq!(reloaded[0].volume, 40);
    }

    #[test]
    fn settings_default_to_local_playback_disabled() {
        let settings: Settings = toml::from_str("").unwrap();

        assert!(!settings.local_playback);
        assert!(!settings.audio.onboarding_complete);
        assert!(settings.audio.mix_sink.is_none());
    }

    #[test]
    fn persists_local_playback_setting() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("settings.toml");

        save_settings_to(
            &path,
            &Settings {
                local_playback: true,
                audio: AudioSettings::default(),
            },
        )
        .unwrap();

        assert!(load_settings_from(&path).unwrap().local_playback);
    }

    #[test]
    fn persists_audio_settings_and_loads_old_settings() {
        let old: Settings = toml::from_str("local_playback = true").unwrap();
        assert!(old.local_playback);
        assert!(old.audio.mix_sink.is_none());

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("settings.toml");
        let settings = Settings {
            local_playback: false,
            audio: AudioSettings {
                onboarding_complete: true,
                mix_sink: Some("soundboard_tui_mix".to_string()),
                virtual_source: Some("soundboard_tui_mix.monitor".to_string()),
                physical_source: Some("alsa_input.usb".to_string()),
                managed_mix: true,
            },
        };

        save_settings_to(&path, &settings).unwrap();
        let loaded = load_settings_from(&path).unwrap();

        assert!(loaded.audio.onboarding_complete);
        assert_eq!(loaded.audio.mix_sink.as_deref(), Some("soundboard_tui_mix"));
        assert!(loaded.audio.managed_mix);
    }

    #[test]
    fn imports_audio_and_resolves_file_name_conflicts() {
        let temporary = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let first = sources.path().join("bonk.mp3");
        let second_directory = sources.path().join("other");
        fs::create_dir(&second_directory).unwrap();
        let second = second_directory.join("bonk.mp3");
        let unsupported = sources.path().join("notes.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        fs::write(&unsupported, b"not audio").unwrap();

        let report = import_sounds_into(
            temporary.path(),
            &[first.clone(), second.clone(), unsupported],
        )
        .unwrap();

        assert_eq!(report.imported, 2);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            fs::read(temporary.path().join("sounds/bonk.mp3")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(temporary.path().join("sounds/bonk (1).mp3")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn deletes_only_files_from_the_managed_sounds_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let sounds_dir = temporary.path().join("sounds");
        fs::create_dir(&sounds_dir).unwrap();
        let managed = sounds_dir.join("bonk.mp3");
        let outside = temporary.path().join("outside.mp3");
        fs::write(&managed, b"managed").unwrap();
        fs::write(&outside, b"outside").unwrap();

        delete_sound_from(temporary.path(), &managed).unwrap();

        assert!(!managed.exists());
        assert_eq!(
            delete_sound_from(temporary.path(), &outside)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(outside.exists());
    }
}
