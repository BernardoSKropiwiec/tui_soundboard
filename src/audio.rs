use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

pub const MANAGED_SINK_NAME: &str = "soundboard_tui_mix";
pub const MANAGED_SOURCE_NAME: &str = "soundboard_tui_mix.monitor";

const MANAGED_MARKER: &str = "# Managed by soundboard_tui";
const CONFIG_FILE_NAME: &str = "90-soundboard-tui.conf";
const SINK_WAIT_ATTEMPTS: usize = 10;
const SINK_WAIT_DELAY: Duration = Duration::from_millis(100);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct AudioNode {
    pub name: String,
    pub description: String,
    pub is_virtual: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AudioInventory {
    pub physical_sources: Vec<AudioNode>,
    pub sinks: Vec<AudioNode>,
    pub virtual_sources: Vec<AudioNode>,
}

#[derive(Clone, Debug, Default)]
pub struct Diagnostics {
    pub pipewire_error: Option<String>,
    pub pulse_error: Option<String>,
    pub mpv_error: Option<String>,
    pub inventory: AudioInventory,
}

impl Diagnostics {
    pub fn playback_ready(&self) -> bool {
        self.pipewire_error.is_none() && self.mpv_error.is_none()
    }

    pub fn creation_ready(&self) -> bool {
        self.pipewire_error.is_none() && self.pulse_error.is_none()
    }

    pub fn errors(&self) -> Vec<&str> {
        [
            self.pipewire_error.as_deref(),
            self.pulse_error.as_deref(),
            self.mpv_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub fn has_sink(&self, name: &str) -> bool {
        self.inventory.sinks.iter().any(|sink| sink.name == name)
    }
}

pub fn diagnose() -> Diagnostics {
    let (inventory, pipewire_error) = match run(&mut Command::new("pw-dump")) {
        Ok(output) => match parse_pw_dump(&output.stdout) {
            Ok(inventory) => (inventory, None),
            Err(error) => (
                AudioInventory::default(),
                Some(format!("could not parse pw-dump output: {error}")),
            ),
        },
        Err(error) => (AudioInventory::default(), Some(error)),
    };

    let pulse_error = match run(Command::new("pactl").arg("info")) {
        Ok(output) => {
            let info = String::from_utf8_lossy(&output.stdout);
            if info.to_ascii_lowercase().contains("pipewire") {
                None
            } else {
                Some("pactl is available, but its server is not PipeWire".to_string())
            }
        }
        Err(error) => Some(error),
    };

    let mpv_error = match run(Command::new("mpv").arg("--version")) {
        Err(error) => Some(error),
        Ok(_) => match run(Command::new("mpv").args(["--no-config", "--ao=help"])) {
            Ok(output) => {
                let mut text = output.stdout;
                text.extend_from_slice(&output.stderr);
                if String::from_utf8_lossy(&text)
                    .to_ascii_lowercase()
                    .contains("pipewire")
                {
                    None
                } else {
                    Some("mpv does not report PipeWire audio output support".to_string())
                }
            }
            Err(error) => Some(error),
        },
    };

    Diagnostics {
        pipewire_error,
        pulse_error,
        mpv_error,
        inventory,
    }
}

pub fn install_managed_mix(source_name: &str) -> io::Result<()> {
    validate_source_name(source_name)?;

    let path = managed_config_path()?;
    protect_unmanaged_file(&path)?;
    let previous_contents = match fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("managed PipeWire config path has no parent"))?;
    fs::create_dir_all(parent)?;
    atomic_write(&path, render_config(source_name).as_bytes())?;

    let result = restart_pipewire_pulse().and_then(|()| wait_for_managed_mix(source_name));
    if let Err(error) = result {
        let rollback = restore_config(&path, previous_contents.as_deref())
            .and_then(|()| restart_pipewire_pulse());
        return Err(match rollback {
            Ok(()) => io::Error::new(error.kind(), format!("{error}; previous setup restored")),
            Err(rollback_error) => io::Error::new(
                error.kind(),
                format!("{error}; rollback also failed: {rollback_error}"),
            ),
        });
    }

    Ok(())
}

pub fn remove_managed_mix() -> io::Result<()> {
    let path = managed_config_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if !has_managed_marker(&contents) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to remove unmanaged PipeWire config: {}",
                path.display()
            ),
        ));
    }

    fs::remove_file(&path)?;
    if let Err(error) = restart_pipewire_pulse() {
        let rollback =
            atomic_write(&path, contents.as_bytes()).and_then(|()| restart_pipewire_pulse());
        return Err(match rollback {
            Ok(()) => io::Error::new(error.kind(), format!("{error}; removal rolled back")),
            Err(rollback_error) => io::Error::new(
                error.kind(),
                format!("{error}; removal rollback also failed: {rollback_error}"),
            ),
        });
    }
    Ok(())
}

pub fn managed_config_path() -> io::Result<PathBuf> {
    let config_home =
        if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
            PathBuf::from(path)
        } else {
            let home = env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither XDG_CONFIG_HOME nor HOME is set",
                    )
                })?;
            PathBuf::from(home).join(".config")
        };

    Ok(config_home
        .join("pipewire")
        .join("pipewire-pulse.conf.d")
        .join(CONFIG_FILE_NAME))
}

pub fn managed_mix_installed() -> io::Result<bool> {
    match fs::read_to_string(managed_config_path()?) {
        Ok(contents) => Ok(has_managed_marker(&contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn managed_mix_ready(source_name: &str, diagnostics: &Diagnostics) -> io::Result<bool> {
    let source_ready = diagnostics
        .inventory
        .virtual_sources
        .iter()
        .any(|source| source.name == MANAGED_SOURCE_NAME);
    Ok(diagnostics.has_sink(MANAGED_SINK_NAME)
        && source_ready
        && managed_loopback_loaded(source_name)?)
}

pub fn sources_for_sink(diagnostics: &Diagnostics, sink_name: &str) -> io::Result<Vec<AudioNode>> {
    let output =
        run(Command::new("pactl").args(["list", "modules", "short"])).map_err(io::Error::other)?;
    let modules = String::from_utf8_lossy(&output.stdout);
    Ok(diagnostics
        .inventory
        .virtual_sources
        .iter()
        .filter(|source| source_matches_modules(&modules, sink_name, &source.name))
        .cloned()
        .collect())
}

fn source_matches_modules(modules: &str, sink_name: &str, source_name: &str) -> bool {
    let direct_monitor = format!("{sink_name}.monitor");
    source_name == direct_monitor
        || modules.lines().any(|line| {
            line.contains("module-remap-source")
                && line.contains(&format!("master={direct_monitor}"))
                && line.contains(&format!("source_name={source_name}"))
        })
}

pub fn source_matches_sink(
    diagnostics: &Diagnostics,
    sink_name: &str,
    source_name: &str,
) -> io::Result<bool> {
    Ok(sources_for_sink(diagnostics, sink_name)?
        .iter()
        .any(|source| source.name == source_name))
}

fn run(command: &mut Command) -> Result<Output, String> {
    let display = format!("{command:?}");
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {display}: {error}"))?;
    let mut child_stdout = child.stdout.take().unwrap();
    let mut child_stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        child_stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = Vec::new();
        child_stderr.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < COMMAND_TIMEOUT => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("{display} timed out after 5 seconds"));
            }
            Err(error) => return Err(format!("could not wait for {display}: {error}")),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("stdout reader for {display} panicked"))?
        .map_err(|error| format!("could not read stdout from {display}: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| format!("stderr reader for {display} panicked"))?
        .map_err(|error| format!("could not read stderr from {display}: {error}"))?;
    let output = Output {
        status,
        stdout,
        stderr,
    };

    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        };
        Err(format!("{display} failed: {detail}"))
    }
}

fn wait_for_managed_mix(source_name: &str) -> io::Result<()> {
    let mut last_error = None;
    for attempt in 0..SINK_WAIT_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(SINK_WAIT_DELAY);
        }
        let diagnostics = diagnose();
        match managed_mix_ready(source_name, &diagnostics) {
            Ok(true) => return Ok(()),
            Ok(false) => last_error = diagnostics.pipewire_error,
            Err(error) => last_error = Some(error.to_string()),
        }
    }

    let detail = last_error
        .map(|error| format!(": {error}"))
        .unwrap_or_default();
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("PipeWire restarted, but the managed mix was incomplete{detail}"),
    ))
}

fn managed_loopback_loaded(source_name: &str) -> io::Result<bool> {
    let output =
        run(Command::new("pactl").args(["list", "modules", "short"])).map_err(io::Error::other)?;
    let modules = String::from_utf8_lossy(&output.stdout);
    Ok(modules.lines().any(|line| {
        line.contains("module-loopback")
            && line.contains(&format!("source={source_name}"))
            && line.contains(&format!("sink={MANAGED_SINK_NAME}"))
    }))
}

fn restore_config(path: &Path, contents: Option<&[u8]>) -> io::Result<()> {
    match contents {
        Some(contents) => atomic_write(path, contents),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn parse_pw_dump(bytes: &[u8]) -> Result<AudioInventory, serde_json::Error> {
    let dump: Value = serde_json::from_slice(bytes)?;
    let mut inventory = AudioInventory::default();

    if let Some(objects) = dump.as_array() {
        for object in objects {
            let Some(props) = object.pointer("/info/props").and_then(Value::as_object) else {
                continue;
            };
            let Some(media_class) = string_property(props.get("media.class")) else {
                continue;
            };
            let Some(name) = string_property(props.get("node.name")) else {
                continue;
            };
            let description = string_property(props.get("node.description"))
                .or_else(|| string_property(props.get("node.nick")))
                .unwrap_or(name)
                .to_string();
            let node = AudioNode {
                name: name.to_string(),
                description,
                is_virtual: source_is_virtual(media_class, name, props)
                    || (media_class == "Audio/Sink"
                        && (truthy_property(props.get("node.virtual"))
                            || string_property(props.get("factory.name"))
                                .is_some_and(|factory| factory.contains("null-audio"))
                            || (!props.contains_key("device.id")
                                && !props.contains_key("device.api")))),
            };

            if media_class == "Audio/Sink" {
                inventory.sinks.push(node);
            } else if media_class == "Audio/Source" || media_class == "Audio/Source/Virtual" {
                if node.is_virtual {
                    inventory.virtual_sources.push(node);
                } else {
                    inventory.physical_sources.push(node);
                }
            }
        }
    }

    inventory
        .physical_sources
        .sort_by(|a, b| a.name.cmp(&b.name));
    inventory.sinks.sort_by(|a, b| a.name.cmp(&b.name));
    inventory
        .virtual_sources
        .sort_by(|a, b| a.name.cmp(&b.name));
    Ok(inventory)
}

fn string_property(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn source_is_virtual(
    media_class: &str,
    name: &str,
    props: &serde_json::Map<String, Value>,
) -> bool {
    media_class == "Audio/Source/Virtual"
        || name.ends_with(".monitor")
        || truthy_property(props.get("node.virtual"))
        || string_property(props.get("factory.name"))
            .is_some_and(|factory| factory.contains("null-audio"))
        || props.contains_key("monitor.channel-volumes")
}

fn truthy_property(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => matches!(value.as_str(), "true" | "1" | "yes"),
        Some(Value::Number(value)) => value.as_u64().is_some_and(|value| value != 0),
        _ => false,
    }
}

fn validate_source_name(source_name: &str) -> io::Result<()> {
    if !source_name.is_empty()
        && source_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source name must match [A-Za-z0-9_.:-]+",
        ))
    }
}

fn render_config(source_name: &str) -> String {
    format!(
        "{MANAGED_MARKER}\n\
pulse.cmd = [\n\
    {{ cmd = \"load-module\" args = \"module-null-sink sink_name={MANAGED_SINK_NAME} sink_properties=device.description=Soundboard_TUI_Mix\" flags = [ ] }}\n\
    {{ cmd = \"load-module\" args = \"module-loopback source={source_name} sink={MANAGED_SINK_NAME}\" flags = [ ] }}\n\
]\n"
    )
}

fn has_managed_marker(contents: &str) -> bool {
    contents
        .strip_prefix(MANAGED_MARKER)
        .is_some_and(|remainder| remainder.starts_with('\n') || remainder.starts_with("\r\n"))
}

fn protect_unmanaged_file(path: &Path) -> io::Result<()> {
    match fs::read_to_string(path) {
        Ok(contents) if has_managed_marker(&contents) => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite unmanaged PipeWire config: {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("config path has no parent"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..100_u32 {
        let temporary = parent.join(format!(
            ".{CONFIG_FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            nonce + u128::from(attempt)
        ));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = (|| {
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            fs::File::open(parent)?.sync_all()
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary config file",
    ))
}

fn restart_pipewire_pulse() -> io::Result<()> {
    let systemctl_error = match run(Command::new("systemctl").args([
        "--user",
        "restart",
        "pipewire-pulse.service",
    ])) {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };

    match run(Command::new("pactl").arg("exit")) {
        Ok(_) => Ok(()),
        Err(pactl_error) => Err(io::Error::other(format!(
            "could not restart PipeWire Pulse: {systemctl_error}; fallback failed: {pactl_error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_classifies_pw_dump_nodes() {
        let dump = br#"[
            {"info":{"props":{"media.class":"Audio/Source","node.name":"alsa_input.usb","node.description":"USB microphone"}}},
            {"info":{"props":{"media.class":"Audio/Sink","node.name":"alsa_output.pci","node.nick":"Speakers"}}},
            {"info":{"props":{"media.class":"Audio/Source","node.name":"alsa_output.pci.monitor","node.description":"Monitor"}}},
            {"info":{"props":{"media.class":"Audio/Source/Virtual","node.name":"virtual_mic"}}},
            {"info":{"props":{"media.class":"Stream/Output/Audio","node.name":"ignored"}}}
        ]"#;

        let inventory = parse_pw_dump(dump).unwrap();
        assert_eq!(inventory.physical_sources.len(), 1);
        assert_eq!(inventory.physical_sources[0].name, "alsa_input.usb");
        assert_eq!(inventory.sinks.len(), 1);
        assert_eq!(inventory.sinks[0].description, "Speakers");
        assert_eq!(inventory.virtual_sources.len(), 2);
        assert_eq!(inventory.virtual_sources[0].name, "alsa_output.pci.monitor");
        assert_eq!(inventory.virtual_sources[1].description, "virtual_mic");
    }

    #[test]
    fn validates_source_names_strictly() {
        for valid in ["alsa_input.usb-1:2", "microphone_1", "source.name"] {
            assert!(validate_source_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "source name", "source/name", "source\nname", "ç"] {
            assert!(validate_source_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn renders_managed_modules() {
        let config = render_config("alsa_input.usb");
        assert!(config.starts_with(MANAGED_MARKER));
        assert!(config.contains("module-null-sink sink_name=soundboard_tui_mix"));
        assert!(config.contains("module-loopback source=alsa_input.usb sink=soundboard_tui_mix"));
    }

    #[test]
    fn refuses_to_replace_a_file_without_the_marker() {
        let directory = env::temp_dir().join(format!(
            "soundboard-tui-audio-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "pulse.cmd = [ ]\n").unwrap();

        let error = protect_unmanaged_file(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!has_managed_marker("prefix # Managed by soundboard_tui\n"));

        fs::write(&path, render_config("source.name")).unwrap();
        assert!(protect_unmanaged_file(&path).is_ok());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn associates_direct_and_remapped_sources_with_a_sink() {
        let modules = "536870916 module-null-sink sink_name=discord_mix\n\
            536870918 module-remap-source master=discord_mix.monitor source_name=discord_mic";

        assert!(source_matches_modules(
            modules,
            "discord_mix",
            "discord_mix.monitor"
        ));
        assert!(source_matches_modules(
            modules,
            "discord_mix",
            "discord_mic"
        ));
        assert!(!source_matches_modules(
            modules,
            "discord_mix",
            "unrelated_source"
        ));
    }
}
