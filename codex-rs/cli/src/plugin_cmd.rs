use anyhow::Result;
use anyhow::ensure;
use clap::Parser;
use codex_core::config::find_codex_home;
use codex_core::plugins::PluginStore;
use std::path::PathBuf;

#[derive(Debug, Parser)]
pub struct PluginCli {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PluginCommand {
    /// Install a local plugin bundle. Restart Codex afterward.
    Add(PluginArgs),
    /// List installed plugins.
    List(ListArgs),
    /// Remove a plugin and its cached bundle.
    Remove(PluginArgs),
    /// Enable an installed plugin for new sessions.
    Enable(PluginArgs),
    /// Disable an installed plugin for new sessions.
    Disable(PluginArgs),
    /// Register, list, or remove local marketplace directories.
    Marketplace(MarketplaceCli),
}

#[derive(Debug, Parser)]
struct PluginArgs {
    /// PLUGIN@MARKETPLACE, or PLUGIN with --marketplace.
    plugin: String,
    #[arg(long = "marketplace", short = 'm')]
    marketplace: Option<String>,
    #[arg(long)]
    json: bool,
}

impl PluginArgs {
    fn id(&self) -> Result<String> {
        match &self.marketplace {
            Some(marketplace) => {
                ensure!(
                    !self.plugin.contains('@'),
                    "specify the marketplace only once"
                );
                Ok(format!("{}@{marketplace}", self.plugin))
            }
            None => Ok(self.plugin.clone()),
        }
    }
}

#[derive(Debug, Parser)]
struct ListArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
struct MarketplaceCli {
    #[command(subcommand)]
    command: MarketplaceCommand,
}

#[derive(Debug, clap::Subcommand)]
enum MarketplaceCommand {
    /// Register a directory containing .agents/plugins/marketplace.json.
    Add {
        path: PathBuf,
    },
    List(ListArgs),
    /// Remove a marketplace after removing its installed plugins.
    Remove {
        name: String,
    },
}

impl PluginCli {
    pub fn run(self) -> Result<()> {
        let store = PluginStore::new(find_codex_home()?);
        match self.command {
            PluginCommand::Add(args) => {
                let plugin = store.install(&args.id()?)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&plugin)?);
                } else {
                    println!("Added plugin `{}`.", plugin.id);
                    println!("Installed plugin root: {}", plugin.installed_path.display());
                    println!("Restart Codex to use its skills and MCP servers.");
                }
            }
            PluginCommand::List(args) => {
                let plugins = store.list()?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&plugins)?);
                } else if plugins.is_empty() {
                    println!(
                        "No plugins installed. Run `codex plugin marketplace add PATH` first."
                    );
                } else {
                    for plugin in plugins {
                        let state = if plugin.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        };
                        println!("{} ({state}) - {}", plugin.id, plugin.display_name);
                        if let Some(error) = plugin.error {
                            println!("  {error}");
                        }
                    }
                }
            }
            PluginCommand::Remove(args) => {
                let id = args.id()?;
                store.remove(&id)?;
                print_change(&id, "removed", args.json)?;
            }
            PluginCommand::Enable(args) => {
                let id = args.id()?;
                store.set_enabled(&id, true)?;
                print_change(&id, "enabled", args.json)?;
            }
            PluginCommand::Disable(args) => {
                let id = args.id()?;
                store.set_enabled(&id, false)?;
                print_change(&id, "disabled", args.json)?;
            }
            PluginCommand::Marketplace(marketplace) => match marketplace.command {
                MarketplaceCommand::Add { path } => {
                    let name = store.add_marketplace(&path)?;
                    println!("Added local marketplace `{name}`.");
                }
                MarketplaceCommand::List(args) => {
                    let marketplaces = store.marketplaces()?;
                    if args.json {
                        println!("{}", serde_json::to_string_pretty(&marketplaces)?);
                    } else if marketplaces.is_empty() {
                        println!("No local marketplaces configured.");
                    } else {
                        for (name, config) in marketplaces {
                            println!("{name}: {}", config.source.display());
                        }
                    }
                }
                MarketplaceCommand::Remove { name } => {
                    store.remove_marketplace(&name)?;
                    println!("Removed marketplace `{name}`.");
                }
            },
        }
        Ok(())
    }
}

fn print_change(id: &str, state: &str, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": id,
                "state": state,
            }))?
        );
    } else {
        println!("Plugin `{id}` {state}. Restart Codex for changes to take effect.");
    }
    Ok(())
}
