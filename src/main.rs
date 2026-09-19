mod audio;
mod library;
mod player;
mod terminal;

use std::{
    collections::BTreeSet,
    env, fs, io,
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use library::Sound;
use terminal::TerminalSession;

const LIBRARY_WIDTH: u16 = 32;
const PAD_HEIGHT: u16 = 5;
const MIN_PAD_WIDTH: u16 = 20;
const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5);
const PLAYBACK_POLL_INTERVAL: Duration = Duration::from_millis(200);

// Um enum garante que apenas um modo esteja ativo por vez.
#[derive(Clone, Copy)]
enum Mode {
    Configuration,
    Use,
}

struct FileEntry {
    path: PathBuf,
    is_directory: bool,
}

struct FilePicker {
    directory: PathBuf,
    entries: Vec<FileEntry>,
    query: String,
    selected_index: usize,
    selected_paths: BTreeSet<PathBuf>,
}

impl FilePicker {
    fn new() -> io::Result<Self> {
        let directory = env::var_os("HOME")
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(env::current_dir)?;
        let mut picker = Self {
            directory,
            entries: Vec::new(),
            query: String::new(),
            selected_index: 0,
            selected_paths: BTreeSet::new(),
        };
        picker.refresh()?;
        Ok(picker)
    }

    fn refresh(&mut self) -> io::Result<()> {
        let mut entries = Vec::new();

        for entry in fs::read_dir(&self.directory)? {
            let path = entry?.path();
            let is_directory = path.is_dir();
            if is_directory || (path.is_file() && library::is_supported_audio(&path)) {
                entries.push(FileEntry { path, is_directory });
            }
        }

        entries.sort_by(|left, right| {
            right
                .is_directory
                .cmp(&left.is_directory)
                .then_with(|| left.path.file_name().cmp(&right.path.file_name()))
        });
        self.entries = entries;
        self.selected_index = self
            .selected_index
            .min(self.visible_entry_count().saturating_sub(1));
        Ok(())
    }

    fn change_directory(&mut self, directory: PathBuf) -> io::Result<()> {
        let previous_directory = std::mem::replace(&mut self.directory, directory);
        let previous_query = std::mem::take(&mut self.query);
        self.selected_index = 0;
        if let Err(error) = self.refresh() {
            self.directory = previous_directory;
            self.query = previous_query;
            self.refresh()?;
            return Err(error);
        }
        Ok(())
    }

    fn selected_entry(&self) -> Option<&FileEntry> {
        self.visible_entries().nth(self.selected_index)
    }

    fn visible_entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|entry| file_entry_matches(entry, &self.query))
    }

    fn visible_entry_count(&self) -> usize {
        self.visible_entries().count()
    }

    fn push_query(&mut self, character: char) {
        self.query.push(character);
        self.selected_index = 0;
    }

    fn pop_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }

        self.query.pop();
        self.selected_index = 0;
        true
    }

    fn toggle_selected_file(&mut self) {
        let Some(path) = self
            .selected_entry()
            .filter(|entry| !entry.is_directory)
            .map(|entry| entry.path.clone())
        else {
            return;
        };

        if !self.selected_paths.remove(&path) {
            self.selected_paths.insert(path);
        }
    }
}

fn file_entry_matches(entry: &FileEntry, query: &str) -> bool {
    let Some(name) = entry.path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();

    if name.starts_with('.') && !query.starts_with('.') {
        return false;
    }

    query.is_empty() || name.to_lowercase().contains(&query.to_lowercase())
}

struct StatusMessage {
    text: String,
    is_error: bool,
    expires_at: Option<Instant>,
}

struct DeleteConfirmation {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum MetadataField {
    Name,
    Aliases,
}

struct MetadataEditor {
    sound_index: usize,
    name: String,
    aliases: String,
    field: MetadataField,
}

impl MetadataEditor {
    fn new(sound_index: usize, sound: &Sound) -> Self {
        Self {
            sound_index,
            name: sound.name.clone(),
            aliases: sound.aliases.join(", "),
            field: MetadataField::Name,
        }
    }

    fn active_value_mut(&mut self) -> &mut String {
        match self.field {
            MetadataField::Name => &mut self.name,
            MetadataField::Aliases => &mut self.aliases,
        }
    }

    fn toggle_field(&mut self) {
        self.field = match self.field {
            MetadataField::Name => MetadataField::Aliases,
            MetadataField::Aliases => MetadataField::Name,
        };
    }
}

#[derive(Clone, Copy)]
enum AudioSetupStep {
    Diagnostics,
    Destination,
    VirtualSource,
    PhysicalSource,
    ConfirmCreation,
}

struct AudioSetup {
    diagnostics: audio::Diagnostics,
    step: AudioSetupStep,
    selected_index: usize,
    selected_sink: Option<String>,
    selected_source: Option<String>,
    compatible_sources: Vec<audio::AudioNode>,
    required: bool,
    error: Option<String>,
}

impl AudioSetup {
    fn new(required: bool) -> Self {
        let diagnostics = audio::diagnose();
        let selected_index = preferred_destination_index(&diagnostics);
        Self {
            diagnostics,
            step: AudioSetupStep::Diagnostics,
            selected_index,
            selected_sink: None,
            selected_source: None,
            compatible_sources: Vec::new(),
            required,
            error: None,
        }
    }

    fn virtual_sinks(&self) -> Vec<&audio::AudioNode> {
        self.diagnostics
            .inventory
            .sinks
            .iter()
            .filter(|sink| sink.is_virtual)
            .collect()
    }

    fn destination_count(&self) -> usize {
        1 + self.virtual_sinks().len()
    }

    fn selection_count(&self) -> usize {
        match self.step {
            AudioSetupStep::Diagnostics | AudioSetupStep::ConfirmCreation => 1,
            AudioSetupStep::Destination => self.destination_count(),
            AudioSetupStep::VirtualSource => self.compatible_sources.len().max(1),
            AudioSetupStep::PhysicalSource => {
                self.diagnostics.inventory.physical_sources.len().max(1)
            }
        }
    }

    fn refresh(&mut self) {
        self.diagnostics = audio::diagnose();
        self.selected_index = preferred_destination_index(&self.diagnostics);
        self.selected_sink = None;
        self.selected_source = None;
        self.compatible_sources.clear();
        self.error = None;
    }
}

fn preferred_destination_index(diagnostics: &audio::Diagnostics) -> usize {
    usize::from(
        diagnostics
            .inventory
            .sinks
            .iter()
            .any(|sink| sink.is_virtual),
    )
}

// Fonte unica do estado que sobrevive entre redesenhos.
struct App {
    mode: Mode,
    query: String,
    selected_index: usize,
    sounds: Vec<Sound>,
    settings: library::Settings,
    playing_path: Option<PathBuf>,
    audio_setup: Option<AudioSetup>,
    file_picker: Option<FilePicker>,
    delete_confirmation: Option<DeleteConfirmation>,
    metadata_editor: Option<MetadataEditor>,
    status_message: Option<StatusMessage>,
    should_quit: bool,
}

impl App {
    fn new(mode: Mode) -> io::Result<Self> {
        let settings = library::load_settings()?;
        let diagnostics = audio::diagnose();
        let managed_config_ready = !settings.audio.managed_mix
            || (audio::managed_mix_installed().unwrap_or(false)
                && settings
                    .audio
                    .physical_source
                    .as_deref()
                    .is_some_and(|source| {
                        audio::managed_mix_ready(source, &diagnostics).unwrap_or(false)
                    }));
        let virtual_source_ready =
            settings
                .audio
                .virtual_source
                .as_deref()
                .is_some_and(|configured| {
                    diagnostics
                        .inventory
                        .virtual_sources
                        .iter()
                        .any(|source| source.name == configured)
                });
        let destination_pair_ready = match (
            settings.audio.mix_sink.as_deref(),
            settings.audio.virtual_source.as_deref(),
        ) {
            (Some(sink), Some(source)) => {
                audio::source_matches_sink(&diagnostics, sink, source).unwrap_or(false)
            }
            _ => false,
        };
        let audio_ready = settings.audio.onboarding_complete
            && settings
                .audio
                .mix_sink
                .as_deref()
                .is_some_and(|sink| diagnostics.has_sink(sink))
            && diagnostics.playback_ready();
        let audio_ready =
            audio_ready && virtual_source_ready && destination_pair_ready && managed_config_ready;
        let preferred_audio_destination = preferred_destination_index(&diagnostics);
        let (playing_path, playback_error) = match player::current_path() {
            Ok(path) => (path, None),
            Err(error) => (None, Some(format!("Could not query mpv: {error}"))),
        };

        Ok(Self {
            mode,
            query: String::new(),
            selected_index: 0,
            sounds: library::load_sounds()?,
            settings,
            playing_path,
            audio_setup: (!audio_ready).then_some(AudioSetup {
                diagnostics,
                step: AudioSetupStep::Diagnostics,
                selected_index: preferred_audio_destination,
                selected_sink: None,
                selected_source: None,
                compatible_sources: Vec::new(),
                required: true,
                error: None,
            }),
            file_picker: None,
            delete_confirmation: None,
            metadata_editor: None,
            status_message: playback_error.map(|text| StatusMessage {
                text,
                is_error: true,
                expires_at: None,
            }),
            should_quit: false,
        })
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Configuration => Mode::Use,
            Mode::Use => Mode::Configuration,
        };
    }

    fn handle_key(&mut self, key: KeyEvent, pad_columns: usize) -> io::Result<()> {
        if self.audio_setup.is_some() {
            self.handle_audio_setup_key(key);
            return Ok(());
        }

        if self.metadata_editor.is_some() {
            self.handle_metadata_editor_key(key);
            return Ok(());
        }

        if self.delete_confirmation.is_some() {
            self.handle_delete_confirmation_key(key)?;
            return Ok(());
        }

        if self.file_picker.is_some() {
            self.handle_file_picker_key(key)?;
            return Ok(());
        }

        match key.code {
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_mode();
            }
            KeyCode::Char('n')
                if matches!(self.mode, Mode::Use)
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.select_next();
            }
            KeyCode::Char('p')
                if matches!(self.mode, Mode::Use)
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.select_previous();
            }
            KeyCode::Down if matches!(self.mode, Mode::Use) => self.select_next(),
            KeyCode::Up if matches!(self.mode, Mode::Use) => self.select_previous(),
            KeyCode::Enter if matches!(self.mode, Mode::Use) => self.play_selected(),
            KeyCode::Right if matches!(self.mode, Mode::Configuration) => self.select_next_pad(1),
            KeyCode::Left if matches!(self.mode, Mode::Configuration) => {
                self.select_previous_pad(1)
            }
            KeyCode::Down if matches!(self.mode, Mode::Configuration) => {
                self.select_next_pad(pad_columns)
            }
            KeyCode::Up if matches!(self.mode, Mode::Configuration) => {
                self.select_previous_pad(pad_columns)
            }
            KeyCode::Enter | KeyCode::Char(' ') if matches!(self.mode, Mode::Configuration) => {
                self.play_pad()
            }
            KeyCode::Char('+') | KeyCode::Char('=') if matches!(self.mode, Mode::Configuration) => {
                self.adjust_volume(5)?
            }
            KeyCode::Char('-') if matches!(self.mode, Mode::Configuration) => {
                self.adjust_volume(-5)?
            }
            KeyCode::Char('m') if matches!(self.mode, Mode::Configuration) => {
                self.toggle_local_playback()?
            }
            KeyCode::Char('A') if matches!(self.mode, Mode::Configuration) => {
                self.audio_setup = Some(AudioSetup::new(false));
                self.status_message = None;
            }
            KeyCode::Char('s') if matches!(self.mode, Mode::Configuration) => self.stop_playback(),
            KeyCode::Char('r') if matches!(self.mode, Mode::Configuration) => {
                self.reload_sounds()?;
            }
            KeyCode::Char('a') if matches!(self.mode, Mode::Configuration) => {
                match FilePicker::new() {
                    Ok(picker) => {
                        self.file_picker = Some(picker);
                        self.status_message = None;
                    }
                    Err(error) => self.set_error(format!("Could not open file picker: {error}")),
                }
            }
            KeyCode::Char('d') if matches!(self.mode, Mode::Configuration) => {
                if let Some(sound) = self.sounds.get(self.selected_index) {
                    self.delete_confirmation = Some(DeleteConfirmation {
                        name: sound.name.clone(),
                        path: sound.path.clone(),
                    });
                    self.status_message = None;
                }
            }
            KeyCode::Char('e') if matches!(self.mode, Mode::Configuration) => {
                if let Some(sound) = self.sounds.get(self.selected_index) {
                    self.metadata_editor = Some(MetadataEditor::new(self.selected_index, sound));
                    self.status_message = None;
                }
            }
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('q') if matches!(self.mode, Mode::Configuration) => {
                self.should_quit = true;
            }
            KeyCode::Char(character)
                if matches!(self.mode, Mode::Use)
                    && (key.modifiers == KeyModifiers::NONE
                        || key.modifiers == KeyModifiers::SHIFT) =>
            {
                self.query.push(character);
                self.selected_index = 0;
            }
            KeyCode::Backspace if matches!(self.mode, Mode::Use) => {
                self.query.pop();
                self.selected_index = 0;
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_audio_setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                let required = self
                    .audio_setup
                    .as_ref()
                    .is_some_and(|setup| setup.required);
                self.audio_setup = None;
                if required {
                    self.set_error(
                        "Audio is not configured. Press A to run audio setup before playing."
                            .to_string(),
                    );
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.audio_setup.as_mut().unwrap().refresh();
            }
            KeyCode::Char('x') | KeyCode::Char('X') if self.settings.audio.managed_mix => {
                let previous_audio_settings = self.settings.audio.clone();
                match audio::remove_managed_mix() {
                    Ok(()) => {
                        self.settings.audio = library::AudioSettings::default();
                        if let Err(error) = library::save_settings(&self.settings) {
                            let rollback = previous_audio_settings
                                .physical_source
                                .as_deref()
                                .ok_or_else(|| {
                                    io::Error::other("saved physical microphone is missing")
                                })
                                .and_then(audio::install_managed_mix);
                            self.settings.audio = previous_audio_settings;
                            self.audio_setup.as_mut().unwrap().error = Some(match rollback {
                                Ok(()) => format!(
                                    "Could not save settings after removal: {error}; mix restored"
                                ),
                                Err(rollback_error) => format!(
                                    "Could not save settings after removal: {error}; could not restore mix: {rollback_error}"
                                ),
                            });
                        } else {
                            let setup = self.audio_setup.as_mut().unwrap();
                            setup.required = true;
                            setup.step = AudioSetupStep::Diagnostics;
                            setup.refresh();
                        }
                    }
                    Err(error) => {
                        self.audio_setup.as_mut().unwrap().error =
                            Some(format!("Could not remove the managed mix: {error}"));
                    }
                }
            }
            KeyCode::Down => {
                let setup = self.audio_setup.as_mut().unwrap();
                setup.selected_index =
                    (setup.selected_index + 1).min(setup.selection_count().saturating_sub(1));
            }
            KeyCode::Up => {
                let setup = self.audio_setup.as_mut().unwrap();
                setup.selected_index = setup.selected_index.saturating_sub(1);
            }
            KeyCode::Enter => self.advance_audio_setup(),
            _ => {}
        }
    }

    fn advance_audio_setup(&mut self) {
        let Some(setup) = self.audio_setup.as_ref() else {
            return;
        };
        let step = setup.step;
        let selected_index = setup.selected_index;

        match step {
            AudioSetupStep::Diagnostics => {
                let setup = self.audio_setup.as_mut().unwrap();
                if setup.diagnostics.playback_ready() {
                    setup.step = AudioSetupStep::Destination;
                    setup.selected_index = preferred_destination_index(&setup.diagnostics);
                    setup.error = None;
                } else {
                    setup.error = Some(setup.diagnostics.errors().join("\n"));
                }
            }
            AudioSetupStep::Destination if selected_index == 0 => {
                let setup = self.audio_setup.as_mut().unwrap();
                if !setup.diagnostics.creation_ready() {
                    setup.error =
                        Some(setup.diagnostics.pulse_error.clone().unwrap_or_else(|| {
                            "PipeWire mix creation is unavailable.".to_string()
                        }));
                } else if setup.diagnostics.inventory.physical_sources.is_empty() {
                    setup.error = Some("No physical microphone was detected.".to_string());
                } else {
                    setup.step = AudioSetupStep::PhysicalSource;
                    setup.selected_index = 0;
                    setup.error = None;
                }
            }
            AudioSetupStep::Destination => {
                if self.settings.audio.managed_mix {
                    self.audio_setup.as_mut().unwrap().error = Some(
                        "Remove the managed mix with x before selecting another destination."
                            .to_string(),
                    );
                    return;
                }
                let sink = self
                    .audio_setup
                    .as_ref()
                    .and_then(|setup| setup.virtual_sinks().get(selected_index - 1).copied())
                    .cloned();
                if let Some(sink) = sink {
                    let compatible_sources = match audio::sources_for_sink(
                        &self.audio_setup.as_ref().unwrap().diagnostics,
                        &sink.name,
                    ) {
                        Ok(sources) if !sources.is_empty() => sources,
                        Ok(_) => {
                            self.audio_setup.as_mut().unwrap().error = Some(format!(
                                "No virtual microphone source is connected to {}.",
                                sink.description
                            ));
                            return;
                        }
                        Err(error) => {
                            self.audio_setup.as_mut().unwrap().error = Some(format!(
                                "Could not validate the virtual microphone: {error}"
                            ));
                            return;
                        }
                    };
                    let expected_source = format!("{}.monitor", sink.name);
                    let selected_index = compatible_sources
                        .iter()
                        .position(|source| source.name == expected_source)
                        .unwrap_or(0);
                    let setup = self.audio_setup.as_mut().unwrap();
                    setup.selected_sink = Some(sink.name);
                    setup.compatible_sources = compatible_sources;
                    setup.step = AudioSetupStep::VirtualSource;
                    setup.selected_index = selected_index;
                    setup.error = None;
                }
            }
            AudioSetupStep::VirtualSource => {
                let setup = self.audio_setup.as_ref().unwrap();
                let source = setup
                    .compatible_sources
                    .get(selected_index)
                    .map(|source| source.name.clone());
                let sink = setup.selected_sink.clone();
                match (sink, source) {
                    (Some(sink), Some(source)) => {
                        self.save_audio_setup(library::AudioSettings {
                            onboarding_complete: true,
                            virtual_source: Some(source),
                            mix_sink: Some(sink),
                            physical_source: None,
                            managed_mix: false,
                        });
                    }
                    _ => {
                        self.audio_setup.as_mut().unwrap().error = Some(
                            "No virtual microphone source is available for this destination."
                                .to_string(),
                        );
                    }
                }
            }
            AudioSetupStep::PhysicalSource => {
                let source = self
                    .audio_setup
                    .as_ref()
                    .and_then(|setup| {
                        setup
                            .diagnostics
                            .inventory
                            .physical_sources
                            .get(selected_index)
                    })
                    .map(|source| source.name.clone());
                if let Some(source) = source {
                    let setup = self.audio_setup.as_mut().unwrap();
                    setup.selected_source = Some(source);
                    setup.step = AudioSetupStep::ConfirmCreation;
                    setup.selected_index = 0;
                    setup.error = None;
                }
            }
            AudioSetupStep::ConfirmCreation => {
                let source = self
                    .audio_setup
                    .as_ref()
                    .and_then(|setup| setup.selected_source.clone());
                let Some(source) = source else {
                    return;
                };

                match audio::install_managed_mix(&source) {
                    Ok(()) => self.save_audio_setup(library::AudioSettings {
                        onboarding_complete: true,
                        mix_sink: Some(audio::MANAGED_SINK_NAME.to_string()),
                        virtual_source: Some(audio::MANAGED_SOURCE_NAME.to_string()),
                        physical_source: Some(source),
                        managed_mix: true,
                    }),
                    Err(error) => {
                        let setup = self.audio_setup.as_mut().unwrap();
                        setup.refresh();
                        setup.error = Some(format!("Could not create the managed mix: {error}"));
                    }
                }
            }
        }
    }

    fn save_audio_setup(&mut self, audio_settings: library::AudioSettings) {
        let previous = std::mem::replace(&mut self.settings.audio, audio_settings);
        match library::save_settings(&self.settings) {
            Ok(()) => {
                let sink = self.settings.audio.mix_sink.clone().unwrap_or_default();
                self.audio_setup = None;
                self.set_temporary_status(format!("Audio destination configured: {sink}."));
            }
            Err(error) => {
                let rollback = match (self.settings.audio.managed_mix, previous.managed_mix) {
                    (true, true) => previous
                        .physical_source
                        .as_deref()
                        .ok_or_else(|| io::Error::other("previous microphone is missing"))
                        .and_then(audio::install_managed_mix),
                    (true, false) => audio::remove_managed_mix(),
                    _ => Ok(()),
                };
                self.settings.audio = previous;
                self.audio_setup.as_mut().unwrap().error = Some(match rollback {
                    Err(rollback_error) => format!(
                        "Could not save audio settings: {error}; could not remove the new mix: {rollback_error}"
                    ),
                    Ok(()) => {
                        format!("Could not save audio settings: {error}; audio setup restored")
                    }
                });
            }
        }
    }

    fn handle_metadata_editor_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.metadata_editor = None;
                self.status_message = None;
            }
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                self.metadata_editor.as_mut().unwrap().toggle_field();
            }
            KeyCode::Enter => self.save_metadata(),
            KeyCode::Backspace => {
                self.metadata_editor
                    .as_mut()
                    .unwrap()
                    .active_value_mut()
                    .pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.metadata_editor
                    .as_mut()
                    .unwrap()
                    .active_value_mut()
                    .clear();
            }
            KeyCode::Char(character)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.metadata_editor
                    .as_mut()
                    .unwrap()
                    .active_value_mut()
                    .push(character);
            }
            _ => {}
        }
    }

    fn save_metadata(&mut self) {
        let Some(editor) = self.metadata_editor.as_ref() else {
            return;
        };
        let name = editor.name.trim().to_string();
        if name.is_empty() {
            self.set_error("Name cannot be empty.".to_string());
            return;
        }

        let sound_index = editor.sound_index;
        let aliases = parse_aliases(&editor.aliases);
        let Some(sound) = self.sounds.get_mut(sound_index) else {
            self.metadata_editor = None;
            self.set_error("The selected audio is no longer available.".to_string());
            return;
        };
        let previous_name = std::mem::replace(&mut sound.name, name.clone());
        let previous_aliases = std::mem::replace(&mut sound.aliases, aliases);

        match library::save_sounds(&self.sounds) {
            Ok(()) => {
                self.metadata_editor = None;
                self.set_temporary_status(format!("Updated audio '{name}'."));
            }
            Err(error) => {
                self.sounds[sound_index].name = previous_name;
                self.sounds[sound_index].aliases = previous_aliases;
                self.set_error(format!("Could not save audio metadata: {error}"));
            }
        }
    }

    fn handle_delete_confirmation_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.delete_selected_sound()?
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.delete_confirmation = None;
            }
            _ => {}
        }

        Ok(())
    }

    fn delete_selected_sound(&mut self) -> io::Result<()> {
        let Some(confirmation) = self.delete_confirmation.take() else {
            return Ok(());
        };
        let selected_index = self.selected_index;

        if let Err(error) = library::delete_sound(&confirmation.path) {
            self.set_error(format!("Could not delete '{}': {error}", confirmation.name));
            return Ok(());
        }

        match library::load_sounds() {
            Ok(sounds) => {
                self.sounds = sounds;
                self.selected_index = selected_index.min(self.sounds.len().saturating_sub(1));
                self.set_temporary_status(format!("Deleted audio '{}'.", confirmation.name));
            }
            Err(error) => self.set_error(format!(
                "Deleted '{}', but could not reload the library: {error}",
                confirmation.name
            )),
        }

        Ok(())
    }

    fn handle_file_picker_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match key.code {
            KeyCode::Esc => self.file_picker = None,
            KeyCode::Down => {
                let picker = self.file_picker.as_mut().unwrap();
                let entry_count = picker.visible_entry_count();
                if entry_count > 0 {
                    picker.selected_index = (picker.selected_index + 1).min(entry_count - 1);
                }
            }
            KeyCode::Up => {
                let picker = self.file_picker.as_mut().unwrap();
                picker.selected_index = picker.selected_index.saturating_sub(1);
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.import_selected_files()?
            }
            KeyCode::Tab => self.import_selected_files()?,
            KeyCode::Enter => {
                let entry = self
                    .file_picker
                    .as_ref()
                    .and_then(FilePicker::selected_entry)
                    .map(|entry| (entry.path.clone(), entry.is_directory));

                match entry {
                    Some((path, true)) => self.change_picker_directory(path),
                    Some((_, false)) => self.file_picker.as_mut().unwrap().toggle_selected_file(),
                    None => {}
                }
            }
            KeyCode::Backspace => {
                if self.file_picker.as_mut().unwrap().pop_query() {
                    return Ok(());
                }

                let parent = self
                    .file_picker
                    .as_ref()
                    .and_then(|picker| picker.directory.parent())
                    .map(PathBuf::from);
                if let Some(parent) = parent {
                    self.change_picker_directory(parent);
                }
            }
            KeyCode::Char(character)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.file_picker.as_mut().unwrap().push_query(character);
            }
            _ => {}
        }

        Ok(())
    }

    fn change_picker_directory(&mut self, directory: PathBuf) {
        if let Err(error) = self
            .file_picker
            .as_mut()
            .unwrap()
            .change_directory(directory)
        {
            self.set_error(format!("Could not open directory: {error}"));
        }
    }

    fn import_selected_files(&mut self) -> io::Result<()> {
        let paths = self
            .file_picker
            .as_ref()
            .map(|picker| picker.selected_paths.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        if paths.is_empty() {
            self.set_error("Select at least one audio file".to_string());
            return Ok(());
        }

        let report = match library::import_sounds(&paths) {
            Ok(report) => report,
            Err(error) => {
                self.set_error(format!("Could not access the managed library: {error}"));
                return Ok(());
            }
        };

        self.file_picker = None;
        if report.imported > 0
            && let Err(error) = self.reload_sounds()
        {
            self.set_error(format!(
                "Imported {} file(s), but could not reload the library: {error}",
                report.imported
            ));
            return Ok(());
        }

        if report.errors.is_empty() {
            self.set_temporary_status(format!("Imported {} audio file(s).", report.imported));
        } else {
            let details = report.errors.join("\n");
            self.set_error(format!(
                "Imported {} file(s); {} failed:\n{details}",
                report.imported,
                report.errors.len()
            ));
        }

        Ok(())
    }

    fn set_error(&mut self, text: String) {
        self.status_message = Some(StatusMessage {
            text,
            is_error: true,
            expires_at: None,
        });
    }

    fn set_temporary_status(&mut self, text: String) {
        self.status_message = Some(StatusMessage {
            text,
            is_error: false,
            expires_at: Some(Instant::now() + STATUS_MESSAGE_DURATION),
        });
    }

    fn clear_expired_status(&mut self) {
        let expired = self
            .status_message
            .as_ref()
            .and_then(|message| message.expires_at)
            .is_some_and(|expires_at| expires_at <= Instant::now());

        if expired {
            self.status_message = None;
        }
    }

    fn status_timeout(&self) -> Option<Duration> {
        self.status_message
            .as_ref()
            .and_then(|message| message.expires_at)
            .map(|expires_at| expires_at.saturating_duration_since(Instant::now()))
    }

    fn select_next(&mut self) {
        let result_count = self.filtered_sounds().len();

        if result_count > 0 {
            self.selected_index = (self.selected_index + 1).min(result_count - 1);
        }
    }

    fn select_previous(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    fn play_selected(&mut self) {
        let sound = self
            .filtered_sounds()
            .get(self.selected_index)
            .map(|sound| (sound.path.clone(), sound.volume, sound.name.clone()));

        if let Some((path, volume, name)) = sound {
            let Some(mix_sink) = self.settings.audio.mix_sink.as_deref() else {
                self.set_error("Configure an audio destination before playing.".to_string());
                return;
            };
            match player::play(&path, volume, self.settings.local_playback, mix_sink) {
                Ok(outcome) if outcome.local_error.is_none() => {
                    self.playing_path = Some(path);
                    self.should_quit = true;
                }
                Ok(outcome) => {
                    self.playing_path = Some(path);
                    self.set_error(format!(
                        "Audio was sent to the virtual microphone, but local playback failed: {}",
                        outcome.local_error.unwrap()
                    ));
                }
                Err(error) => self.set_error(format!("Could not play '{name}': {error}")),
            }
        }
    }

    fn reload_sounds(&mut self) -> io::Result<()> {
        self.sounds = library::load_sounds()?;
        self.selected_index = 0;
        Ok(())
    }

    fn select_next_pad(&mut self, amount: usize) {
        if !self.sounds.is_empty() {
            self.selected_index = (self.selected_index + amount).min(self.sounds.len() - 1);
        }
    }

    fn select_previous_pad(&mut self, amount: usize) {
        self.selected_index = self.selected_index.saturating_sub(amount);
    }

    fn play_pad(&mut self) {
        let sound = self
            .sounds
            .get(self.selected_index)
            .map(|sound| (sound.path.clone(), sound.volume, sound.name.clone()));

        if let Some((path, volume, name)) = sound {
            let Some(mix_sink) = self.settings.audio.mix_sink.as_deref() else {
                self.set_error("Configure an audio destination before playing.".to_string());
                return;
            };
            match player::play(&path, volume, self.settings.local_playback, mix_sink) {
                Ok(outcome) if outcome.local_error.is_none() => {
                    self.playing_path = Some(path);
                    self.set_temporary_status(format!("Playing '{name}'."));
                }
                Ok(outcome) => {
                    self.playing_path = Some(path);
                    self.set_error(format!(
                        "Playing '{name}' on the virtual microphone, but local playback failed: {}",
                        outcome.local_error.unwrap()
                    ));
                }
                Err(error) => self.set_error(format!("Could not play '{name}': {error}")),
            }
        }
    }

    fn stop_playback(&mut self) {
        match player::stop() {
            Ok(()) => {
                self.playing_path = None;
                self.set_temporary_status("Playback stopped.".to_string());
            }
            Err(error) => self.set_error(format!("Could not stop playback: {error}")),
        }
    }

    fn refresh_playback_state(&mut self) {
        match player::current_path() {
            Ok(path) => self.playing_path = path,
            Err(error) => {
                let already_reported = self
                    .status_message
                    .as_ref()
                    .is_some_and(|message| message.text.starts_with("Could not query mpv:"));
                if !already_reported {
                    self.set_error(format!("Could not query mpv: {error}"));
                }
            }
        }
    }

    fn adjust_volume(&mut self, delta: i8) -> io::Result<()> {
        let Some(sound) = self.sounds.get_mut(self.selected_index) else {
            return Ok(());
        };
        let previous_volume = sound.volume;
        sound.volume = adjusted_volume(sound.volume, delta);

        if sound.volume == previous_volume {
            return Ok(());
        }

        if let Err(error) = library::save_sounds(&self.sounds) {
            self.sounds[self.selected_index].volume = previous_volume;
            return Err(error);
        }

        Ok(())
    }

    fn toggle_local_playback(&mut self) -> io::Result<()> {
        let local_playback = !self.settings.local_playback;
        self.settings.local_playback = local_playback;
        if let Err(error) = library::save_settings(&self.settings) {
            self.settings.local_playback = !local_playback;
            return Err(error);
        }

        if !local_playback && let Err(error) = player::stop_local() {
            self.set_error(format!("Could not stop local playback: {error}"));
        }

        Ok(())
    }

    fn filtered_sounds(&self) -> Vec<&Sound> {
        let query = self.query.to_lowercase();

        // Retorna referencias para nao copiar os sons durante cada busca.
        self.sounds
            .iter()
            .filter(|sound| {
                query.is_empty()
                    || sound.name.to_lowercase().contains(&query)
                    || sound.file_name().to_lowercase().contains(&query)
                    || sound
                        .aliases
                        .iter()
                        .any(|alias| alias.to_lowercase().contains(&query))
            })
            .collect()
    }
}

fn main() -> io::Result<()> {
    let initial_mode = if env::args().any(|argument| argument == "--quick") {
        Mode::Use
    } else {
        Mode::Configuration
    };

    let mut session = TerminalSession::new()?;
    let run_result = run(session.terminal_mut(), initial_mode);
    let restore_result = session.restore();
    run_result.and(restore_result)
}

fn run(
    terminal: &mut Terminal<impl Backend<Error = io::Error>>,
    initial_mode: Mode,
) -> io::Result<()> {
    let mut app = App::new(initial_mode)?;

    // Cada evento altera o estado e causa um novo desenho.
    while !app.should_quit {
        app.clear_expired_status();
        app.refresh_playback_state();
        terminal.draw(|frame| draw(frame, &app))?;

        let timeout = app
            .status_timeout()
            .map_or(PLAYBACK_POLL_INTERVAL, |status_timeout| {
                status_timeout.min(PLAYBACK_POLL_INTERVAL)
            });
        if !event::poll(timeout)? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let width = terminal.size()?.width;
            app.handle_key(key, pad_columns(width.saturating_sub(LIBRARY_WIDTH)))?;
        }
    }

    Ok(())
}

fn draw(frame: &mut Frame, app: &App) {
    match app.mode {
        Mode::Configuration => draw_configuration(frame, app),
        Mode::Use => draw_use(frame, app),
    }

    if let Some(picker) = &app.file_picker {
        draw_file_picker(frame, picker, app.status_message.as_ref());
    }
    if let Some(confirmation) = &app.delete_confirmation {
        draw_delete_confirmation(frame, confirmation);
    }
    if let Some(editor) = &app.metadata_editor {
        draw_metadata_editor(frame, editor, app.status_message.as_ref());
    }
    if let Some(setup) = &app.audio_setup {
        draw_audio_setup(frame, setup, &app.settings.audio);
    }
}

fn draw_audio_setup(
    frame: &mut Frame,
    setup: &AudioSetup,
    current_settings: &library::AudioSettings,
) {
    let area = centered_percentage_area(82, 82, frame.area());
    let mut lines = Vec::new();
    let title = match setup.step {
        AudioSetupStep::Diagnostics => {
            lines.push(status_line(
                "PipeWire",
                setup.diagnostics.pipewire_error.as_deref(),
            ));
            lines.push(status_line(
                "pipewire-pulse",
                setup.diagnostics.pulse_error.as_deref(),
            ));
            lines.push(status_line("mpv", setup.diagnostics.mpv_error.as_deref()));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Detected: {} physical microphone(s), {} virtual sink(s)",
                setup.diagnostics.inventory.physical_sources.len(),
                setup.virtual_sinks().len()
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Enter  Continue    r  Run diagnostics again    Esc  Close/Exit",
            ));
            " AUDIO DIAGNOSTICS "
        }
        AudioSetupStep::Destination => {
            lines.push(selectable_line(
                selected_index_is(setup, 0),
                "Create Soundboard TUI virtual microphone",
            ));
            for (index, sink) in setup.virtual_sinks().iter().enumerate() {
                lines.push(selectable_line(
                    selected_index_is(setup, index + 1),
                    &format!("Use existing: {} ({})", sink.description, sink.name),
                ));
            }
            if setup.virtual_sinks().is_empty() {
                lines.push(
                    Line::from("No existing virtual sinks detected.")
                        .style(Style::new().fg(Color::DarkGray)),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Up/Down  Select    Enter  Confirm    Esc  Close/Exit",
            ));
            " SELECT AUDIO DESTINATION "
        }
        AudioSetupStep::VirtualSource => {
            for (index, source) in setup.compatible_sources.iter().enumerate() {
                lines.push(selectable_line(
                    selected_index_is(setup, index),
                    &format!("{} ({})", source.description, source.name),
                ));
            }
            if setup.compatible_sources.is_empty() {
                lines.push(
                    Line::from("No virtual microphone sources detected.")
                        .style(Style::new().fg(Color::Red)),
                );
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Choose the microphone source exposed to Discord or another voice app.",
            ));
            lines.push(Line::from(
                "Up/Down  Select    Enter  Confirm    Esc  Close",
            ));
            " SELECT VIRTUAL MICROPHONE "
        }
        AudioSetupStep::PhysicalSource => {
            for (index, source) in setup
                .diagnostics
                .inventory
                .physical_sources
                .iter()
                .enumerate()
            {
                lines.push(selectable_line(
                    selected_index_is(setup, index),
                    &format!("{} ({})", source.description, source.name),
                ));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "This microphone will be mixed with soundboard audio.",
            ));
            lines.push(Line::from(
                "Up/Down  Select    Enter  Continue    Esc  Close/Exit",
            ));
            " SELECT PHYSICAL MICROPHONE "
        }
        AudioSetupStep::ConfirmCreation => {
            let config_path = audio::managed_config_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| format!("unavailable: {error}"));
            lines.push(Line::from("The application will:"));
            lines.push(Line::from(format!("  Create {config_path}")));
            lines.push(Line::from(format!(
                "  Mix {} with soundboard audio",
                setup.selected_source.as_deref().unwrap_or("unknown source")
            )));
            lines.push(Line::from("  Restart the user pipewire-pulse service"));
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Select {} as your microphone in Discord or another voice app.",
                audio::MANAGED_SOURCE_NAME
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("Enter  Create and connect    Esc  Cancel"));
            " CONFIRM VIRTUAL MICROPHONE "
        }
    };

    if current_settings.managed_mix {
        lines.push(
            Line::from("x  Remove the managed virtual microphone")
                .style(Style::new().fg(Color::Red)),
        );
    }
    if let Some(error) = &setup.error {
        lines.push(Line::from(""));
        lines.extend(
            error
                .lines()
                .map(|line| Line::from(line.to_string()).style(Style::new().fg(Color::Red))),
        );
    }

    let dialog = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().fg(Color::Cyan)),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(dialog, area);
}

fn status_line<'a>(label: &str, error: Option<&'a str>) -> Line<'a> {
    match error {
        Some(error) => Line::from(vec![
            Span::styled(format!("[FAIL] {label}: "), Style::new().fg(Color::Red)),
            Span::raw(error),
        ]),
        None => Line::from(Span::styled(
            format!("[ OK ] {label}"),
            Style::new().fg(Color::Green),
        )),
    }
}

fn selectable_line(selected: bool, text: &str) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    Line::from(Span::styled(format!("{marker} {text}"), style))
}

fn selected_index_is(setup: &AudioSetup, index: usize) -> bool {
    setup.selected_index == index
}

fn draw_configuration(frame: &mut Frame, app: &App) {
    // Layout calcula areas; os widgets sao desenhados depois nelas.
    let footer_height = if app.status_message.is_some() { 5 } else { 3 };
    let [header_area, content_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(footer_height),
    ])
    .areas(frame.area());

    let [library_area, pads_area] =
        Layout::horizontal([Constraint::Length(LIBRARY_WIDTH), Constraint::Min(0)])
            .areas(content_area);

    let local_status = if app.settings.local_playback {
        "ON"
    } else {
        "OFF"
    };
    let audio_status = app
        .settings
        .audio
        .mix_sink
        .as_deref()
        .unwrap_or("NOT CONFIGURED");
    let header = Paragraph::new(format!(
        " CONFIGURATION  AUDIO {audio_status}  LOCAL MONITOR {local_status} "
    ))
    .block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(Color::Cyan)),
    );
    let library_items = app.sounds.iter().map(|sound| {
        let playing = app.playing_path.as_ref() == Some(&sound.path);
        let marker = if playing { "*" } else { " " };
        ListItem::new(format!(
            " {marker} {}  {}%\n    {}",
            sound.name,
            sound.volume,
            sound.file_name()
        ))
    });
    let library = List::new(library_items).block(Block::bordered().title(" LIBRARY "));
    let footer = if let Some(message) = &app.status_message {
        let color = if message.is_error {
            Color::Red
        } else {
            Color::Green
        };
        Paragraph::new(message.text.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" STATUS ")
                    .border_style(Style::new().fg(color)),
            )
    } else {
        Paragraph::new(
            " Arrows Select  Enter/Space Play  s Stop  +/- Volume  a Add  e Edit  d Delete  m Monitor  A Audio setup  r Reload  Ctrl+k Quick play  q Exit ",
        )
        .block(Block::bordered().border_style(Style::new().fg(Color::DarkGray)))
    };

    frame.render_widget(header, header_area);
    frame.render_widget(library, library_area);
    draw_pads(frame, app, pads_area);
    frame.render_widget(footer, footer_area);
}

fn draw_metadata_editor(
    frame: &mut Frame,
    editor: &MetadataEditor,
    status: Option<&StatusMessage>,
) {
    let area = centered_area(70, 12, frame.area());
    let name_style = if matches!(editor.field, MetadataField::Name) {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::new()
    };
    let aliases_style = if matches!(editor.field, MetadataField::Aliases) {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::new()
    };
    let mut content = vec![
        Line::from(vec![
            Span::raw("Name:    "),
            Span::styled(format!("{}_", editor.name), name_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("Aliases: "),
            Span::styled(format!("{}_", editor.aliases), aliases_style),
        ]),
        Line::from("         Separate aliases with commas."),
        Line::from(""),
    ];
    if let Some(status) = status.filter(|status| status.is_error) {
        content.push(Line::from(status.text.as_str()).style(Style::new().fg(Color::Red)));
    } else {
        content.push(Line::from(
            "Tab/Up/Down  Field    Ctrl+u  Clear    Enter  Save    Esc  Cancel",
        ));
    }
    let dialog = Paragraph::new(content).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .title(" EDIT AUDIO ")
            .border_style(Style::new().fg(Color::Cyan)),
    );

    frame.render_widget(Clear, area);
    frame.render_widget(dialog, area);
}

fn draw_delete_confirmation(frame: &mut Frame, confirmation: &DeleteConfirmation) {
    let area = centered_area(60, 7, frame.area());
    let content = vec![
        Line::from(format!("Delete '{}' permanently?", confirmation.name)),
        Line::from(confirmation.path.to_string_lossy()).style(Style::new().fg(Color::DarkGray)),
        Line::from("Enter/y  Delete    Esc/n  Cancel"),
    ];
    let dialog = Paragraph::new(content)
        .centered()
        .wrap(Wrap { trim: true })
        .block(
            Block::bordered()
                .title(" CONFIRM DELETE ")
                .border_style(Style::new().fg(Color::Red)),
        );

    frame.render_widget(Clear, area);
    frame.render_widget(dialog, area);
}

fn draw_file_picker(frame: &mut Frame, picker: &FilePicker, status: Option<&StatusMessage>) {
    let area = centered_percentage_area(85, 85, frame.area());
    let [path_area, entries_area, help_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(area);
    let title = format!(" ADD AUDIO  {} selected ", picker.selected_paths.len());
    let path = Paragraph::new(vec![
        Line::from(picker.directory.to_string_lossy()),
        Line::from(format!("Filter: {}_", picker.query)),
    ])
    .block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().fg(Color::Cyan)),
    );
    let visible_entries = picker.visible_entries().collect::<Vec<_>>();
    let visible_rows = usize::from(entries_area.height.saturating_sub(2)).max(1);
    let window_start =
        centered_window_start(picker.selected_index, visible_entries.len(), visible_rows);
    let window_end = (window_start + visible_rows).min(visible_entries.len());
    let items = visible_entries[window_start..window_end]
        .iter()
        .map(|entry| {
            let name = entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            if entry.is_directory {
                ListItem::new(format!("  {name}/"))
            } else {
                let marker = if picker.selected_paths.contains(&entry.path) {
                    "[x]"
                } else {
                    "[ ]"
                };
                ListItem::new(format!("{marker} {name}"))
            }
        });
    let entries = List::new(items)
        .block(Block::bordered().title(format!(
            " DIRECTORIES AND AUDIO FILES  {} matches ",
            visible_entries.len()
        )))
        .highlight_symbol("> ")
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let help = if let Some(status) = status.filter(|status| status.is_error) {
        Paragraph::new(status.text.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" ERROR ")
                    .border_style(Style::new().fg(Color::Red)),
            )
    } else {
        Paragraph::new(
            " Type Filter  Up/Down Select  Enter Open/Mark  Backspace Erase/Parent  Tab Import  Esc Cancel ",
        )
        .block(Block::bordered())
    };
    let mut state = ListState::default();
    if !visible_entries.is_empty() {
        state.select(Some(picker.selected_index - window_start));
    }

    frame.render_widget(Clear, area);
    frame.render_widget(path, path_area);
    frame.render_stateful_widget(entries, entries_area, &mut state);
    frame.render_widget(help, help_area);
}

fn centered_window_start(selected: usize, item_count: usize, visible_rows: usize) -> usize {
    if item_count <= visible_rows {
        return 0;
    }

    selected
        .saturating_sub(visible_rows / 2)
        .min(item_count - visible_rows)
}

fn draw_pads(frame: &mut Frame, app: &App, area: Rect) {
    let columns = pad_columns(area.width);
    let rows = usize::from(area.height.saturating_sub(2) / PAD_HEIGHT).max(1);
    let page_size = columns * rows;
    let page_start = (app.selected_index / page_size) * page_size;
    let page_end = (page_start + page_size).min(app.sounds.len());
    let page = page_start / page_size + 1;
    let page_count = app.sounds.len().div_ceil(page_size).max(1);
    let title = format!(" SOUND PADS  {page}/{page_count} ");
    let block = Block::bordered().title(title);
    let inner = block.inner(area);

    frame.render_widget(block, area);

    if app.sounds.is_empty() {
        frame.render_widget(
            Paragraph::new("No audio files. Add files to the managed sounds directory.")
                .style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let row_areas = Layout::vertical(vec![Constraint::Length(PAD_HEIGHT); rows]).split(inner);
    for (position, sound_index) in (page_start..page_end).enumerate() {
        let row = position / columns;
        let column = position % columns;
        let column_areas = Layout::horizontal(vec![Constraint::Ratio(1, columns as u32); columns])
            .split(row_areas[row]);
        let sound = &app.sounds[sound_index];
        let selected = sound_index == app.selected_index;
        let playing = app.playing_path.as_ref() == Some(&sound.path);
        let border_style = if playing {
            Style::new().fg(Color::Green)
        } else if selected {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        let style = if selected {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let playback_status = if playing { "PLAYING" } else { "" };
        let content = vec![
            Line::from(Span::styled(sound.name.as_str(), style)),
            Line::from(Span::styled(format!("VOL {:>3}%", sound.volume), style)),
            Line::from(Span::styled(
                playback_status,
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )),
        ];
        let pad = Paragraph::new(content)
            .centered()
            .block(Block::bordered().border_style(border_style));

        frame.render_widget(pad, column_areas[column]);
    }
}

fn draw_use(frame: &mut Frame, app: &App) {
    let footer_height = if app.status_message.is_some() { 4 } else { 1 };
    let area = centered_area(70, 13 + footer_height, frame.area());
    let [search_area, results_area, help_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(footer_height),
    ])
    .areas(area);

    let search = Paragraph::new(format!("> {}_", app.query)).block(
        Block::bordered()
            .title(" QUICK PLAY ")
            .border_style(Style::new().fg(Color::Yellow)),
    );
    let result_items = app
        .filtered_sounds()
        .into_iter()
        .map(|sound| ListItem::new(format!("{}  [{}]", sound.name, sound.file_name())));
    let results = List::new(result_items)
        .block(Block::bordered().title(" RESULTS "))
        .highlight_symbol("> ")
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let mut result_state = ListState::default();
    if !app.filtered_sounds().is_empty() {
        result_state.select(Some(app.selected_index));
    }
    let help = if let Some(message) = &app.status_message {
        let color = if message.is_error {
            Color::Red
        } else {
            Color::Green
        };
        Paragraph::new(message.text.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::bordered()
                    .title(" STATUS ")
                    .border_style(Style::new().fg(color)),
            )
    } else {
        Paragraph::new(" Enter  Play    Up/Down  Select    Ctrl+k  Config    Esc  Close ")
    };

    frame.render_widget(search, search_area);
    frame.render_stateful_widget(results, results_area, &mut result_state);
    frame.render_widget(help, help_area);
}

fn centered_area(width_percent: u16, height: u16, area: Rect) -> Rect {
    // Centraliza primeiro na vertical e depois na horizontal.
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [centered] = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);

    centered
}

fn centered_percentage_area(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [centered] = Layout::horizontal([Constraint::Percentage(width_percent)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);

    centered
}

fn pad_columns(width: u16) -> usize {
    usize::from((width.saturating_sub(2) / MIN_PAD_WIDTH).max(1))
}

fn parse_aliases(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(str::to_string)
        .collect()
}

fn adjusted_volume(volume: u8, delta: i8) -> u8 {
    (volume as i16 + delta as i16).clamp(0, 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjusts_volume_within_valid_range() {
        assert_eq!(adjusted_volume(50, 5), 55);
        assert_eq!(adjusted_volume(100, 5), 100);
        assert_eq!(adjusted_volume(0, -5), 0);
    }

    #[test]
    fn parses_comma_separated_aliases() {
        assert_eq!(
            parse_aliases(" impact, meme , , hammer "),
            ["impact", "meme", "hammer"]
        );
        assert!(parse_aliases(" , ").is_empty());
    }

    #[test]
    fn calculates_responsive_pad_columns() {
        assert_eq!(pad_columns(18), 1);
        assert_eq!(pad_columns(42), 2);
        assert_eq!(pad_columns(82), 4);
    }

    #[test]
    fn filters_file_entries_case_insensitively() {
        let entry = FileEntry {
            path: PathBuf::from("/tmp/Cartoon-Hammer.mp3"),
            is_directory: false,
        };
        let hidden_entry = FileEntry {
            path: PathBuf::from("/tmp/.Hidden-Sound.mp3"),
            is_directory: false,
        };

        assert!(file_entry_matches(&entry, "hammer"));
        assert!(file_entry_matches(&entry, "CARTOON"));
        assert!(!file_entry_matches(&entry, "airhorn"));
        assert!(!file_entry_matches(&hidden_entry, ""));
        assert!(!file_entry_matches(&hidden_entry, "hidden"));
        assert!(file_entry_matches(&hidden_entry, ".hidden"));
    }

    #[test]
    fn keeps_file_picker_selection_around_the_middle() {
        assert_eq!(centered_window_start(0, 20, 7), 0);
        assert_eq!(centered_window_start(8, 20, 7), 5);
        assert_eq!(centered_window_start(19, 20, 7), 13);
        assert_eq!(centered_window_start(3, 5, 7), 0);
    }
}
