use cdp_fs::cdp::CdpClient;
use cdp_fs::fs::CdpFs;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "cdp-fs",
    author = "cdp-fs developers",
    version = "0.1.0",
    about = "FUSE filesystem for Chrome DevTools Protocol (CDP) tabs"
)]
pub struct Cli {
    /// Directory to mount the filesystem on
    #[arg(value_name = "MOUNTPOINT")]
    pub mountpoint: PathBuf,

    /// Chrome DevTools Protocol debugging endpoint URL
    #[arg(
        short = 'e',
        long = "endpoint",
        default_value = "http://localhost:9222",
        help = "CDP endpoint URL"
    )]
    pub endpoint: String,

    /// Allow other users to access this filesystem
    #[arg(long = "allow-other", help = "Enable FUSE allow_other mount option")]
    pub allow_other: bool,

    /// Automatically unmount on process exit (requires user_allow_other in /etc/fuse.conf)
    #[arg(
        long = "auto-unmount",
        default_value_t = false,
        help = "Automatically unmount when filesystem terminates (requires user_allow_other in /etc/fuse.conf)"
    )]
    pub auto_unmount: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    // Ensure mountpoint exists
    if !cli.mountpoint.exists() {
        std::fs::create_dir_all(&cli.mountpoint)?;
    }

    println!("======================================================");
    println!(" cdp-fs: Chrome DevTools Protocol FUSE Filesystem");
    println!(" Mountpoint: {}", cli.mountpoint.display());
    println!(" CDP Endpoint: {}", cli.endpoint);
    println!("======================================================");

    let client = CdpClient::new(cli.endpoint.clone());

    if client.is_running() {
        match client.list_tabs() {
            Ok(tabs) => {
                println!("✓ Connected to Chrome CDP! Found {} tab(s):", tabs.len());
                for (i, tab) in tabs.iter().enumerate() {
                    println!("  [{}] {} ({})", i + 1, tab.title, tab.url);
                }
            }
            Err(e) => {
                println!("Connected to Chrome, but error listing tabs: {e}");
            }
        }
    } else {
        println!(
            "! Notice: Chrome/Chromium is not currently running at {}",
            cli.endpoint
        );
        println!("! Start Chrome with remote debugging enabled:");
        println!("!   google-chrome --remote-debugging-port=9222");
        println!("!   or: chromium --remote-debugging-port=9222");
        println!(
            "! The filesystem will mount now and automatically display tabs when Chrome starts."
        );
    }

    let mut config = fuser::Config::default();
    config
        .mount_options
        .push(fuser::MountOption::FSName("cdp-fs".to_string()));
    config
        .mount_options
        .push(fuser::MountOption::Subtype("cdp-fs".to_string()));
    config
        .mount_options
        .push(fuser::MountOption::DefaultPermissions);

    if cli.auto_unmount {
        config.mount_options.push(fuser::MountOption::AutoUnmount);
        config.acl = fuser::SessionACL::RootAndOwner;
    }
    if cli.allow_other {
        config
            .mount_options
            .push(fuser::MountOption::CUSTOM("allow_other".to_string()));
    }

    println!("\nMounting filesystem on {}...", cli.mountpoint.display());
    println!(
        "Press Ctrl+C or run 'fusermount -u {}' to unmount.\n",
        cli.mountpoint.display()
    );

    let fs = CdpFs::new(client);
    fuser::mount(fs, &cli.mountpoint, &config)?;

    println!("Unmounted successfully.");
    Ok(())
}
