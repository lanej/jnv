use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use clap::Parser;
use crossterm::style::Attribute;
use promkit::{
    jsonz::format::RowFormatter,
    listbox::{self, Listbox},
    text_editor,
};

mod editor;
use editor::{Editor, EditorTheme};
mod config;
mod json;
use json::JsonStreamProvider;
mod processor;
use processor::{
    init::ViewInitializer, monitor::ContextMonitor, spinner::SpinnerSpawner, Context, Processor,
    ViewProvider, Visualizer,
};
mod prompt;
mod render;
use render::{PaneIndex, Renderer, EMPTY_PANE};
mod search;
use search::{IncrementalSearcher, SearchProvider};

use std::sync::OnceLock;

static DEFAULT_CONFIG_FILE: OnceLock<PathBuf> = OnceLock::new();

fn get_config_dir() -> &'static PathBuf {
    DEFAULT_CONFIG_FILE.get_or_init(|| dirs::config_dir().unwrap().join("jnv/config.toml"))
}

/// JSON navigator and interactive filter leveraging jq
#[derive(Parser)]
#[command(
    name = "jnv",
    version,
    help_template = "
{about}

Usage: {usage}

Examples:
- Read from a file:
        {bin} data.json

- Read from standard input:
        cat data.json | {bin}

Arguments:
{positionals}

Options:
{options}
"
)]
pub struct Args {
    /// Optional path to a JSON file.
    /// If not provided or if "-" is specified,
    /// reads from standard input.
    pub input: Option<PathBuf>,

    #[arg(
        short = 'e',
        long = "edit-mode",
        default_value = "insert",
        value_parser = edit_mode_validator,
        help = "Edit mode for the interface ('insert' or 'overwrite').",
        long_help = r#"
        Specifies the edit mode for the interface.
        Acceptable values are "insert" or "overwrite".
        - "insert" inserts a new input at the cursor's position.
        - "overwrite" mode replaces existing characters with new input at the cursor's position.
        "#,
    )]
    pub edit_mode: text_editor::Mode,

    #[arg(
        short = 'i',
        long = "indent",
        default_value = "2",
        help = "Number of spaces used for indentation in the visualized data.",
        long_help = "
        Affect the formatting of the displayed JSON,
        making it more readable by adjusting the indentation level.
        "
    )]
    pub indent: usize,

    #[arg(
        short = 'n',
        long = "no-hint",
        help = "Disables the display of hints.",
        long_help = "
        When this option is enabled, it prevents the display of
        hints that typically guide or offer suggestions to the user.
        "
    )]
    pub no_hint: bool,

    #[arg(
        short = 'c',
        long = "config-file",
        help = "Path to the configuration file.",
        long_help = "
        Specifies the path to the configuration file.
        ",
        default_value = get_config_dir().to_str().unwrap(),
    )]
    pub config_file: String,

    #[arg(
        long = "max-streams",
        help = "Maximum number of JSON streams to display",
        long_help = "
        Sets the maximum number of JSON streams to load and display.
        Limiting this value improves performance for large datasets.
        If not set, all streams will be displayed.
        "
    )]
    pub max_streams: Option<usize>,

    #[arg(
        long = "write-default-config",
        help = "Writes the default configuration to the specified file and exits."
    )]
    pub write_default_config: bool,

    #[arg(
        long = "suggestions",
        default_value = "3",
        help = "Number of autocomplete suggestions to show",
        long_help = "
        Sets the number of autocomplete suggestions displayed during incremental search.
        Higher values show more suggestions but may occupy more screen space.
        Adjust this value based on your screen size and preference.
        "
    )]
    pub suggestions: usize,
}

impl Args {
    pub fn write_default_config(&self) -> anyhow::Result<()> {
        let path = PathBuf::from(&self.config_file);

        if path.exists() {
            return Err(anyhow!("Config file `{}` already exists", path.display()));
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(path)?;
        let default_config: config::ConfigFile = Default::default();
        file.write_all(toml::to_string_pretty(&default_config)?.as_bytes())?;
        Ok(())
    }
}

fn edit_mode_validator(val: &str) -> Result<text_editor::Mode> {
    match val {
        "insert" | "" => Ok(text_editor::Mode::Insert),
        "overwrite" => Ok(text_editor::Mode::Overwrite),
        _ => Err(anyhow!("edit-mode must be 'insert' or 'overwrite'")),
    }
}

/// Parses the input based on the provided arguments.
///
/// This function reads input data from either a specified file or standard input.
/// If the `input` argument is `None`, or if it is a path
/// that equals "-", data is read from standard input.
/// Otherwise, the function attempts to open and
/// read from the file specified in the `input` argument.
fn parse_input(args: &Args) -> Result<String> {
    let mut ret = String::new();

    match &args.input {
        None => {
            io::stdin().read_to_string(&mut ret)?;
        }
        Some(path) => {
            if path == &PathBuf::from("-") {
                io::stdin().read_to_string(&mut ret)?;
            } else {
                File::open(path)?.read_to_string(&mut ret)?;
            }
        }
    }

    Ok(ret)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.write_default_config {
        return args.write_default_config();
    }

    let input = parse_input(&args)?;

    let config = if Path::new(&args.config_file).exists() {
        config::load_file(&args.config_file)?
    } else {
        config::Config::default()
    };

    let config::Config {
        search_result_chunk_size,
        query_debounce_duration,
        resize_debounce_duration,
        search_load_chunk_size,
        active_item_style,
        defocus_prefix,
        focus_prefix,
        move_to_tail,
        word_break_chars,
        backward,
        forward,
        defocus_prefix_style,
        defocus_active_char_style,
        defocus_inactive_char_style,
        focus_prefix_style,
        focus_active_char_style,
        focus_inactive_char_style,
        inactive_item_style,
        completion,
        move_to_head,
        move_to_next_nearest,
        move_to_previous_nearest,
        erase,
        erase_all,
        erase_to_previous_nearest,
        erase_to_next_nearest,
        search_up,
        spin_duration,
        prefix_style,
        active_char_style,
        inactive_char_style,
        curly_brackets_style,
        square_brackets_style,
        key_style,
        string_value_style,
        number_value_style,
        boolean_value_style,
        null_value_style,
    } = config;

    let listbox_state = listbox::State {
        listbox: Listbox::from_displayable(Vec::<String>::new()),
        cursor: String::from("❯ "),
        active_item_style,
        inactive_item_style,
        lines: Some(args.suggestions),
    };

    let searcher = IncrementalSearcher::new(listbox_state, search_result_chunk_size);

    let text_editor_state = text_editor::State {
        texteditor: Default::default(),
        history: Default::default(),
        prefix: focus_prefix.clone(),
        mask: Default::default(),
        prefix_style,
        active_char_style,
        inactive_char_style,
        edit_mode: args.edit_mode,
        word_break_chars,
        lines: Default::default(),
    };

    let editor_focus_theme = EditorTheme {
        prefix: focus_prefix.clone(),
        prefix_style: focus_prefix_style,
        active_char_style: focus_active_char_style,
        inactive_char_style: focus_inactive_char_style,
    };

    let editor_defocus_theme = EditorTheme {
        prefix: defocus_prefix,
        prefix_style: defocus_prefix_style,
        active_char_style: defocus_active_char_style,
        inactive_char_style: defocus_inactive_char_style,
    };

    let provider = &mut JsonStreamProvider::new(
        RowFormatter {
            curly_brackets_style,
            square_brackets_style,
            key_style,
            string_value_style,
            number_value_style,
            boolean_value_style,
            null_value_style,
            active_item_attribute: Attribute::Bold,
            inactive_item_attribute: Attribute::Dim,
            indent: args.indent,
        },
        args.max_streams,
    );

    let item = Box::leak(input.into_boxed_str());

    let loading_suggestions_task = searcher.spawn_load_task(provider, item, search_load_chunk_size);

    let editor_keybinds = editor::Keybinds {
        move_to_tail,
        backward,
        forward,
        completion,
        move_to_head,
        move_to_previous_nearest,
        move_to_next_nearest,
        erase,
        erase_all,
        erase_to_previous_nearest,
        erase_to_next_nearest,
        search_up,
    };

    let editor = Editor::new(
        text_editor_state,
        searcher,
        editor_focus_theme,
        editor_defocus_theme,
        editor_keybinds,
    );

    prompt::run(
        item,
        spin_duration,
        query_debounce_duration,
        resize_debounce_duration,
        provider,
        editor,
        loading_suggestions_task,
        args.no_hint,
    )
    .await?;

    Ok(())
}
