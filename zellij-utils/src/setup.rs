use crate::input::theme::Themes;
use crate::{
    cli::{CliArgs, Command},
    consts::{
        FEATURES, SYSTEM_DEFAULT_CONFIG_DIR, SYSTEM_DEFAULT_DATA_DIR_PREFIX, VERSION,
        ZELLIJ_PROJ_DIR,
    },
    input::{
        config::{Config, ConfigError},
        layout::Layout,
        options::Options,
    },
};
use clap::{Args, IntoApp};
use clap_complete::Shell;
use directories_next::BaseDirs;
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom, fmt::Write as FmtWrite, io::Write, path::Path, path::PathBuf, process,
};

const CONFIG_LOCATION: &str = ".config/zellij";
const CONFIG_NAME: &str = "config.kdl";
static ARROW_SEPARATOR: &str = "";

#[cfg(not(test))]
/// Goes through a predefined list and checks for an already
/// existing config directory, returns the first match
pub fn find_default_config_dir() -> Option<PathBuf> {
    default_config_dirs()
        .into_iter()
        .filter(|p| p.is_some())
        .find(|p| p.clone().unwrap().exists())
        .flatten()
}

#[cfg(test)]
pub fn find_default_config_dir() -> Option<PathBuf> {
    None
}

/// Order in which config directories are checked
fn default_config_dirs() -> Vec<Option<PathBuf>> {
    vec![
        home_config_dir(),
        Some(xdg_config_dir()),
        Some(Path::new(SYSTEM_DEFAULT_CONFIG_DIR).to_path_buf()),
    ]
}

/// Looks for an existing dir, uses that, else returns a
/// dir matching the config spec.
pub fn get_default_data_dir() -> PathBuf {
    [
        xdg_data_dir(),
        Path::new(SYSTEM_DEFAULT_DATA_DIR_PREFIX).join("share/zellij"),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or_else(xdg_data_dir)
}

pub fn xdg_config_dir() -> PathBuf {
    ZELLIJ_PROJ_DIR.config_dir().to_owned()
}

pub fn xdg_data_dir() -> PathBuf {
    ZELLIJ_PROJ_DIR.data_dir().to_owned()
}

pub fn home_config_dir() -> Option<PathBuf> {
    if let Some(user_dirs) = BaseDirs::new() {
        let config_dir = user_dirs.home_dir().join(CONFIG_LOCATION);
        Some(config_dir)
    } else {
        None
    }
}

pub fn get_layout_dir(config_dir: Option<PathBuf>) -> Option<PathBuf> {
    config_dir.map(|dir| dir.join("layouts"))
}

pub fn get_theme_dir(config_dir: Option<PathBuf>) -> Option<PathBuf> {
    config_dir.map(|dir| dir.join("themes"))
}
pub fn dump_asset(asset: &[u8]) -> std::io::Result<()> {
    std::io::stdout().write_all(asset)?;
    Ok(())
}

pub const DEFAULT_CONFIG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/config/default.kdl"
));

pub const DEFAULT_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/default.kdl"
));

pub const STRIDER_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/strider.kdl"
));

pub const NO_STATUS_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/disable-status-bar.kdl"
));

pub const COMPACT_BAR_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/compact.kdl"
));

pub const FISH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.fish"
));

pub const BASH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.bash"
));

pub const ZSH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.zsh"
));

pub const BASH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.bash"
));

pub const FISH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.fish"
));

pub const ZSH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.zsh"
));

pub fn dump_default_config() -> std::io::Result<()> {
    dump_asset(DEFAULT_CONFIG)
}

pub fn dump_specified_layout(layout: &str) -> std::io::Result<()> {
    match layout {
        "strider" => dump_asset(STRIDER_LAYOUT),
        "default" => dump_asset(DEFAULT_LAYOUT),
        "compact" => dump_asset(COMPACT_BAR_LAYOUT),
        "disable-status" => dump_asset(NO_STATUS_LAYOUT),
        not_found => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Layout: {} not found", not_found),
        )),
    }
}

#[derive(Debug, Default, Clone, Args, Serialize, Deserialize)]
pub struct Setup {
    /// Dump the default configuration file to stdout
    #[clap(long, value_parser)]
    pub dump_config: bool,

    /// Disables loading of configuration file at default location,
    /// loads the defaults that zellij ships with
    #[clap(long, value_parser)]
    pub clean: bool,

    /// Checks the configuration of zellij and displays
    /// currently used directories
    #[clap(long, value_parser)]
    pub check: bool,

    /// Dump the specified layout file to stdout
    #[clap(long, value_parser)]
    pub dump_layout: Option<String>,

    /// Generates completion for the specified shell
    #[clap(long, value_name = "SHELL", value_parser)]
    pub generate_completion: Option<String>,

    /// Generates auto-start script for the specified shell
    #[clap(long, value_name = "SHELL", value_parser)]
    pub generate_auto_start: Option<String>,
}

impl Setup {
    /// Entrypoint from main
    /// Merges options from the config file and the command line options
    /// into `[Options]`, the command line options superceeding the layout
    /// file options, superceeding the config file options:
    /// 1. command line options (`zellij options`)
    /// 2. layout options
    ///    (`layout.yaml` / `zellij --layout`)
    /// 3. config options (`config.yaml`)
    pub fn from_cli_args(cli_args: &CliArgs) -> Result<(Config, Layout, Options), ConfigError> {
        let clean = cli_args.should_clean_config();
        // note that this can potentially exit the process
        Setup::handle_setup_commands(cli_args);
        let config = if clean {
            Config::default()
        } else {
            Config::try_from(cli_args)?
        };
        let cli_config_options: Option<Options> =
            if let Some(Command::Options(options)) = cli_args.command.clone() {
                Some(options.into())
            } else {
                None
            };
        let (layout, mut config) =
            Setup::parse_layout_and_override_config(cli_config_options.as_ref(), config, cli_args)?;
        let config_options = match cli_config_options {
            Some(cli_config_options) => config.options.merge(cli_config_options),
            None => config.options.clone(),
        };

        if let Some(theme_dir) = config_options
            .theme_dir
            .clone()
            .or_else(|| get_theme_dir(cli_args.config_dir.clone().or_else(find_default_config_dir)))
        {
            if theme_dir.is_dir() {
                for entry in (theme_dir.read_dir()?).flatten() {
                    if let Some(extension) = entry.path().extension() {
                        if extension == "kdl" {
                            match Themes::from_path(entry.path()) {
                                Ok(themes) => config.themes = config.themes.merge(themes),
                                Err(e) => {
                                    log::error!("error loading theme file: {:?}", e);
                                },
                            }
                        }
                    }
                }
            }
        }

        if let Some(Command::Setup(ref setup)) = &cli_args.command {
            setup
                .from_cli_with_options(cli_args, &config_options)
                .map_or_else(
                    |e| {
                        eprintln!("{:?}", e);
                        process::exit(1);
                    },
                    |_| {},
                );
        };
        Ok((config, layout, config_options))
    }

    /// General setup helpers
    pub fn from_cli(&self) -> std::io::Result<()> {
        if self.clean {
            return Ok(());
        }

        if self.dump_config {
            dump_default_config()?;
            std::process::exit(0);
        }

        if let Some(shell) = &self.generate_completion {
            Self::generate_completion(shell);
            std::process::exit(0);
        }

        if let Some(shell) = &self.generate_auto_start {
            Self::generate_auto_start(shell);
            std::process::exit(0);
        }

        if let Some(layout) = &self.dump_layout {
            dump_specified_layout(layout)?;
            std::process::exit(0);
        }

        Ok(())
    }

    /// Checks the merged configuration
    pub fn from_cli_with_options(
        &self,
        opts: &CliArgs,
        config_options: &Options,
    ) -> std::io::Result<()> {
        if self.check {
            Setup::check_defaults_config(opts, config_options)?;
            std::process::exit(0);
        }
        Ok(())
    }

    pub fn check_defaults_config(opts: &CliArgs, config_options: &Options) -> std::io::Result<()> {
        let data_dir = opts.data_dir.clone().unwrap_or_else(get_default_data_dir);
        let config_dir = opts.config_dir.clone().or_else(find_default_config_dir);
        let plugin_dir = data_dir.join("plugins");
        let layout_dir = config_options
            .layout_dir
            .clone()
            .or_else(|| get_layout_dir(config_dir.clone()));
        let system_data_dir = PathBuf::from(SYSTEM_DEFAULT_DATA_DIR_PREFIX).join("share/zellij");
        let config_file = opts
            .config
            .clone()
            .or_else(|| config_dir.clone().map(|p| p.join(CONFIG_NAME)));

        // according to
        // https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
        let hyperlink_start = "\u{1b}]8;;";
        let hyperlink_mid = "\u{1b}\\";
        let hyperlink_end = "\u{1b}]8;;\u{1b}\\";

        let mut message = String::new();

        writeln!(&mut message, "[Version]: {:?}", VERSION).unwrap();
        if let Some(config_dir) = config_dir {
            writeln!(&mut message, "[CONFIG DIR]: {:?}", config_dir).unwrap();
        } else {
            message.push_str("[CONFIG DIR]: Not Found\n");
            let mut default_config_dirs = default_config_dirs()
                .iter()
                .filter_map(|p| p.clone())
                .collect::<Vec<PathBuf>>();
            default_config_dirs.dedup();
            message.push_str(
                " On your system zellij looks in the following config directories by default:\n",
            );
            for dir in default_config_dirs {
                writeln!(&mut message, " {:?}", dir).unwrap();
            }
        }
        if let Some(config_file) = config_file {
            writeln!(&mut message, "[CONFIG FILE]: {:?}", config_file).unwrap();
            // match Config::new(&config_file) {
            match Config::from_path(&config_file, None) {
                Ok(_) => message.push_str("[CONFIG FILE]: Well defined.\n"),
                Err(e) => writeln!(&mut message, "[CONFIG ERROR]: {}", e).unwrap(),
            }
        } else {
            message.push_str("[CONFIG FILE]: Not Found\n");
            writeln!(
                &mut message,
                " By default zellij looks for a file called [{}] in the configuration directory",
                CONFIG_NAME
            )
            .unwrap();
        }
        writeln!(&mut message, "[DATA DIR]: {:?}", data_dir).unwrap();
        message.push_str(&format!("[PLUGIN DIR]: {:?}\n", plugin_dir));
        if let Some(layout_dir) = layout_dir {
            writeln!(&mut message, "[LAYOUT DIR]: {:?}", layout_dir).unwrap();
        } else {
            message.push_str("[LAYOUT DIR]: Not Found\n");
        }
        writeln!(&mut message, "[SYSTEM DATA DIR]: {:?}", system_data_dir).unwrap();

        writeln!(&mut message, "[ARROW SEPARATOR]: {}", ARROW_SEPARATOR).unwrap();
        message.push_str(" Is the [ARROW_SEPARATOR] displayed correctly?\n");
        message.push_str(" If not you may want to either start zellij with a compatible mode: 'zellij options --simplified-ui true'\n");
        let mut hyperlink_compat = String::new();
        hyperlink_compat.push_str(hyperlink_start);
        hyperlink_compat.push_str("https://zellij.dev/documentation/compatibility.html#the-status-bar-fonts-dont-render-correctly");
        hyperlink_compat.push_str(hyperlink_mid);
        hyperlink_compat.push_str("https://zellij.dev/documentation/compatibility.html#the-status-bar-fonts-dont-render-correctly");
        hyperlink_compat.push_str(hyperlink_end);
        write!(
            &mut message,
            " Or check the font that is in use:\n {}\n",
            hyperlink_compat
        )
        .unwrap();
        message.push_str("[MOUSE INTERACTION]: \n");
        message.push_str(" Can be temporarily disabled through pressing the [SHIFT] key.\n");
        message.push_str(" If that doesn't fix any issues consider to disable the mouse handling of zellij: 'zellij options --disable-mouse-mode'\n");

        let default_editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| String::from("Not set, checked $EDITOR and $VISUAL"));
        writeln!(&mut message, "[DEFAULT EDITOR]: {}", default_editor).unwrap();
        writeln!(&mut message, "[FEATURES]: {:?}", FEATURES).unwrap();
        let mut hyperlink = String::new();
        hyperlink.push_str(hyperlink_start);
        hyperlink.push_str("https://www.zellij.dev/documentation/");
        hyperlink.push_str(hyperlink_mid);
        hyperlink.push_str("zellij.dev/documentation");
        hyperlink.push_str(hyperlink_end);
        writeln!(&mut message, "[DOCUMENTATION]: {}", hyperlink).unwrap();
        //printf '\e]8;;http://example.com\e\\This is a link\e]8;;\e\\\n'

        std::io::stdout().write_all(message.as_bytes())?;

        Ok(())
    }
    fn generate_completion(shell: &str) {
        let shell: Shell = match shell.to_lowercase().parse() {
            Ok(shell) => shell,
            _ => {
                eprintln!("Unsupported shell: {}", shell);
                std::process::exit(1);
            },
        };
        let mut out = std::io::stdout();
        clap_complete::generate(shell, &mut CliArgs::command(), "zellij", &mut out);
        // add shell dependent extra completion
        match shell {
            Shell::Bash => {
                let _ = out.write_all(BASH_EXTRA_COMPLETION);
            },
            Shell::Elvish => {},
            Shell::Fish => {
                let _ = out.write_all(FISH_EXTRA_COMPLETION);
            },
            Shell::PowerShell => {},
            Shell::Zsh => {
                let _ = out.write_all(ZSH_EXTRA_COMPLETION);
            },
            _ => {},
        };
    }

    fn generate_auto_start(shell: &str) {
        let shell: Shell = match shell.to_lowercase().parse() {
            Ok(shell) => shell,
            _ => {
                eprintln!("Unsupported shell: {}", shell);
                std::process::exit(1);
            },
        };

        let mut out = std::io::stdout();
        match shell {
            Shell::Bash => {
                let _ = out.write_all(BASH_AUTO_START_SCRIPT);
            },
            Shell::Fish => {
                let _ = out.write_all(FISH_AUTO_START_SCRIPT);
            },
            Shell::Zsh => {
                let _ = out.write_all(ZSH_AUTO_START_SCRIPT);
            },
            _ => {},
        }
    }
    fn parse_layout_and_override_config(
        cli_config_options: Option<&Options>,
        config: Config,
        cli_args: &CliArgs,
    ) -> Result<(Layout, Config), ConfigError> {
        // find the layout folder relative to which we'll look for our layout
        let layout_dir = cli_config_options
            .as_ref()
            .and_then(|cli_options| cli_options.layout_dir.clone())
            .or_else(|| config.options.layout_dir.clone())
            .or_else(|| {
                get_layout_dir(cli_args.config_dir.clone().or_else(find_default_config_dir))
            });
        // the chosen layout can either be a path relative to the layout_dir or a name of one
        // of our assets, this distinction is made when parsing the layout - TODO: ideally, this
        // logic should not be split up and all the decisions should happen here
        let chosen_layout = cli_args
            .layout
            .clone()
            .or_else(|| {
                cli_config_options
                    .as_ref()
                    .and_then(|cli_options| cli_options.default_layout.clone())
            })
            .or_else(|| config.options.default_layout.clone());
        // we merge-override the config here because the layout might contain configuration
        // that needs to take precedence
        Layout::from_path_or_default(chosen_layout.as_ref(), layout_dir.clone(), config)
    }
    fn handle_setup_commands(cli_args: &CliArgs) {
        if let Some(Command::Setup(ref setup)) = &cli_args.command {
            setup.from_cli().map_or_else(
                |e| {
                    eprintln!("{:?}", e);
                    process::exit(1);
                },
                |_| {},
            );
        };
    }
}

#[cfg(test)]
mod setup_test {
    use super::Setup;
    use crate::cli::{CliArgs, Command};
    use crate::input::options::{CliOptions, Options};
    use insta::assert_snapshot;
    use std::path::PathBuf;

    #[test]
    fn default_config_with_no_cli_arguments() {
        let cli_args = CliArgs::default();
        let (config, layout, options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
        assert_snapshot!(format!("{:#?}", layout));
        assert_snapshot!(format!("{:#?}", options));
    }
    #[test]
    fn cli_arguments_override_config_options() {
        let mut cli_args = CliArgs::default();
        cli_args.command = Some(Command::Options(CliOptions {
            options: Options {
                simplified_ui: Some(true),
                ..Default::default()
            },
            ..Default::default()
        }));
        let (_config, _layout, options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
    }
    #[test]
    fn layout_options_override_config_options() {
        let mut cli_args = CliArgs::default();
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-options.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (_config, layout, options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
        assert_snapshot!(format!("{:#?}", layout));
    }
    #[test]
    fn cli_arguments_override_layout_options() {
        let mut cli_args = CliArgs::default();
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-options.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.command = Some(Command::Options(CliOptions {
            options: Options {
                pane_frames: Some(true),
                ..Default::default()
            },
            ..Default::default()
        }));
        let (_config, layout, options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
        assert_snapshot!(format!("{:#?}", layout));
    }
    #[test]
    fn layout_env_vars_override_config_env_vars() {
        let mut cli_args = CliArgs::default();
        cli_args.config = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/config-with-env-vars.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-env-vars.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (config, _layout, _options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_ui_config_overrides_config_ui_config() {
        let mut cli_args = CliArgs::default();
        cli_args.config = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/config-with-ui-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-ui-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (config, _layout, _options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_plugins_override_config_plugins() {
        let mut cli_args = CliArgs::default();
        cli_args.config = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/config-with-plugins-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-plugins-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (config, _layout, _options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_themes_override_config_themes() {
        let mut cli_args = CliArgs::default();
        cli_args.config = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/config-with-themes-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-themes-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (config, _layout, _options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_keybinds_override_config_keybinds() {
        let mut cli_args = CliArgs::default();
        cli_args.config = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/config-with-keybindings-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        cli_args.layout = Some(PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-keybindings-config.kdl",
            env!("CARGO_MANIFEST_DIR")
        )));
        let (config, _layout, _options) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
}
