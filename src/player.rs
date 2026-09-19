use std::{
    env, fs,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Write},
    os::unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

const START_ATTEMPTS: usize = 40;
const START_RETRY_DELAY: Duration = Duration::from_millis(25);
const IPC_TIMEOUT: Duration = Duration::from_millis(750);
const MIX_SOCKET: &str = "soundboard-tui-mpv.sock";
const LOCAL_SOCKET: &str = "soundboard-tui-local-mpv.sock";

pub struct PlayOutcome {
    pub local_error: Option<String>,
}

pub fn play(
    path: &Path,
    volume: u8,
    local_playback: bool,
    mix_sink: &str,
) -> io::Result<PlayOutcome> {
    let _lock = PlayerLock::acquire()?;
    let audio_device = format!("pipewire/{mix_sink}");
    play_on(path, volume, MIX_SOCKET, Some(&audio_device), "mpv-mix.log")?;

    let local_error = if local_playback {
        play_on(path, volume, LOCAL_SOCKET, None, "mpv-local.log")
            .err()
            .map(|error| error.to_string())
    } else {
        stop_on(LOCAL_SOCKET).err().map(|error| error.to_string())
    };

    Ok(PlayOutcome { local_error })
}

pub fn stop() -> io::Result<()> {
    let _lock = PlayerLock::acquire()?;
    let mix_result = stop_on(MIX_SOCKET);
    let local_result = stop_on(LOCAL_SOCKET);
    mix_result.and(local_result)
}

pub fn stop_local() -> io::Result<()> {
    let _lock = PlayerLock::acquire()?;
    stop_on(LOCAL_SOCKET)
}

struct PlayerLock {
    _file: fs::File,
}

impl PlayerLock {
    fn acquire() -> io::Result<Self> {
        let path = socket_path("soundboard-tui-player.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "another soundboard instance is changing playback",
            ),
            fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self { _file: file })
    }
}

pub fn current_path() -> io::Result<Option<PathBuf>> {
    current_path_on(&socket_path(MIX_SOCKET))
}

fn current_path_on(socket_path: &Path) -> io::Result<Option<PathBuf>> {
    let command = serde_json::json!({
        "command": ["get_property", "path"],
        "request_id": 4
    });
    let response = match send_request(socket_path, &command) {
        Ok(response) => response,
        Err(error) if player_is_not_running(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let error = response_error(&response);

    if error == "property unavailable" {
        return Ok(None);
    }
    if error != "success" {
        return Err(io::Error::other(format!("mpv command failed: {error}")));
    }

    response
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(Some)
        .ok_or_else(|| io::Error::other("mpv returned an invalid playback path"))
}

fn stop_on(socket_name: &str) -> io::Result<()> {
    let command = serde_json::json!({
        "command": ["stop"],
        "request_id": 3
    });
    match send_command(&socket_path(socket_name), &command) {
        Ok(()) => Ok(()),
        Err(error) if player_is_not_running(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn play_on(
    path: &Path,
    volume: u8,
    socket_name: &str,
    audio_device: Option<&str>,
    log_name: &str,
) -> io::Result<()> {
    let socket_path = socket_path(socket_name);

    if let Some(expected_device) = audio_device {
        ensure_audio_device(&socket_path, expected_device)?;
    }

    match send_play_command(&socket_path, path, volume) {
        Ok(()) => return Ok(()),
        Err(error) if player_is_not_running(&error) => {}
        Err(error) => return Err(error),
    }

    remove_stale_socket(&socket_path)?;
    let log_path = log_path(log_name)?;
    let mut child = start_mpv(&socket_path, audio_device, &log_path)?;

    for _ in 0..START_ATTEMPTS {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = remove_stale_socket(&socket_path);
                return Err(startup_error(
                    format!("mpv exited during startup with {status}"),
                    &log_path,
                ));
            }
            Ok(None) => {}
            Err(error) => {
                cleanup_started_player(&mut child, &socket_path);
                return Err(with_log(error, &log_path));
            }
        }

        match send_play_command(&socket_path, path, volume) {
            Ok(()) => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(error) if player_is_not_running(&error) => {
                thread::sleep(START_RETRY_DELAY);
            }
            Err(error) => {
                cleanup_started_player(&mut child, &socket_path);
                return Err(with_log(error, &log_path));
            }
        }
    }

    cleanup_started_player(&mut child, &socket_path);
    Err(startup_error(
        "mpv did not create its IPC socket".to_string(),
        &log_path,
    ))
}

fn cleanup_started_player(child: &mut Child, socket_path: &Path) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = remove_stale_socket(socket_path);
}

fn ensure_audio_device(socket_path: &Path, expected: &str) -> io::Result<()> {
    let command = serde_json::json!({
        "command": ["get_property", "audio-device"],
        "request_id": 5
    });
    let response = match send_request(socket_path, &command) {
        Ok(response) => response,
        Err(error) if player_is_not_running(&error) => return Ok(()),
        Err(error) => return Err(error),
    };

    if response_error(&response) != "success" {
        return Err(io::Error::other(format!(
            "mpv could not report its audio device: {}",
            response_error(&response)
        )));
    }
    if response.get("data").and_then(serde_json::Value::as_str) == Some(expected) {
        return Ok(());
    }

    let quit = serde_json::json!({"command": ["quit"], "request_id": 6});
    send_command(socket_path, &quit)?;
    for _ in 0..START_ATTEMPTS {
        if !socket_path.exists() || UnixStream::connect(socket_path).is_err() {
            return remove_stale_socket(socket_path);
        }
        thread::sleep(START_RETRY_DELAY);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "mpv did not stop after the audio destination changed",
    ))
}

fn socket_path(socket_name: &str) -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(socket_name)
}

fn remove_stale_socket(socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn start_mpv(socket_path: &Path, audio_device: Option<&str>, log_path: &Path) -> io::Result<Child> {
    rotate_log(log_path)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)?;
    let stderr = log.try_clone()?;

    let mut command = Command::new("mpv");
    command
        .arg("--no-config")
        .arg("--idle=yes")
        .arg("--no-terminal")
        .arg("--no-video")
        .arg("--ao=pipewire")
        .arg("--msg-level=all=warn")
        .arg(format!("--log-file={}", log_path.display()))
        // Quick play closes its terminal immediately; keep mpv outside its process group.
        .process_group(0);

    if let Some(audio_device) = audio_device {
        command.arg(format!("--audio-device={audio_device}"));
    }

    command
        .arg(format!("--input-ipc-server={}", socket_path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
}

fn log_path(file_name: &str) -> io::Result<PathBuf> {
    let state_home = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state directory not found"))?;
    let directory = state_home.join("soundboard-tui");
    fs::create_dir_all(&directory)?;
    Ok(directory.join(file_name))
}

fn rotate_log(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() <= 1024 * 1024 {
        return Ok(());
    }

    let old_path = path.with_extension("log.old");
    let _ = fs::remove_file(&old_path);
    fs::rename(path, old_path)
}

fn startup_error(message: String, log_path: &Path) -> io::Error {
    let detail = last_log_line(log_path)
        .map(|line| format!(" Last log message: {line}"))
        .unwrap_or_default();
    io::Error::other(format!("{message}. Log: {}.{detail}", log_path.display()))
}

fn with_log(error: io::Error, log_path: &Path) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{error}. Log: {}", log_path.display()),
    )
}

fn last_log_line(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(sanitize_terminal_text)
}

fn sanitize_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && character != '\t' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn send_play_command(socket_path: &Path, sound_path: &Path, volume: u8) -> io::Result<()> {
    let volume_command = serde_json::json!({
        "command": ["set_property", "volume", volume.min(100)],
        "request_id": 1
    });
    let play_command = serde_json::json!({
        "command": ["loadfile", sound_path.to_string_lossy(), "replace"],
        "request_id": 2
    });

    send_command(socket_path, &volume_command)?;
    send_command(socket_path, &play_command)
}

fn send_command(socket_path: &Path, command: &serde_json::Value) -> io::Result<()> {
    let response = send_request(socket_path, command)?;
    let error = response_error(&response);

    if error == "success" {
        Ok(())
    } else {
        Err(io::Error::other(format!("mpv command failed: {error}")))
    }
}

fn send_request(socket_path: &Path, command: &serde_json::Value) -> io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    serde_json::to_writer(&mut stream, command).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;

    let request_id = command.get("request_id").cloned();
    let mut reader = BufReader::new(stream);
    loop {
        let mut response = String::new();
        if reader.read_line(&mut response)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "mpv closed its IPC socket without a response",
            ));
        }
        let response: serde_json::Value =
            serde_json::from_str(&response).map_err(io::Error::other)?;
        let response_id = response.get("request_id");
        if response_id.is_none() && response.get("event").is_some() {
            continue;
        }
        if response_id.is_none() || response_id == request_id.as_ref() {
            return Ok(response);
        }
    }
}

fn response_error(response: &serde_json::Value) -> &str {
    response
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("invalid mpv response")
}

fn player_is_not_running(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn sends_volume_before_loading_audio() {
        let temporary = tempfile::tempdir().unwrap();
        let socket_path = temporary.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let mut commands = Vec::new();

            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let command = serde_json::from_str::<serde_json::Value>(&request).unwrap();
                let request_id = command["request_id"].clone();
                commands.push(command);
                writeln!(
                    stream,
                    "{{\"error\":\"success\",\"request_id\":{request_id}}}"
                )
                .unwrap();
            }

            commands
        });

        send_play_command(&socket_path, Path::new("/tmp/bonk.mp3"), 55).unwrap();

        let commands = server.join().unwrap();
        assert_eq!(
            commands[0]["command"],
            serde_json::json!(["set_property", "volume", 55])
        );
        assert_eq!(
            commands[1]["command"],
            serde_json::json!(["loadfile", "/tmp/bonk.mp3", "replace"])
        );
    }

    #[test]
    fn skips_events_before_the_matching_response() {
        let temporary = tempfile::tempdir().unwrap();
        let socket_path = temporary.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream
                .write_all(
                    b"{\"event\":\"idle\"}\n{\"data\":\"/tmp/bonk.mp3\",\"error\":\"success\",\"request_id\":4}\n",
                )
                .unwrap();
        });

        assert_eq!(
            current_path_on(&socket_path).unwrap(),
            Some(PathBuf::from("/tmp/bonk.mp3"))
        );
        server.join().unwrap();
    }

    #[test]
    fn reports_no_path_when_playback_is_idle() {
        let temporary = tempfile::tempdir().unwrap();
        let socket_path = temporary.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream
                .write_all(b"{\"error\":\"property unavailable\",\"request_id\":4}\n")
                .unwrap();
        });

        assert_eq!(current_path_on(&socket_path).unwrap(), None);
        server.join().unwrap();
    }

    #[test]
    fn sanitizes_control_characters_from_logs() {
        assert_eq!(
            sanitize_terminal_text("bad\u{1b}[31m\nline"),
            "bad [31m line"
        );
    }
}
