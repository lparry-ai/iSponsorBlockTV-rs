//! Interactive command-line setup (port of `config_setup.py`).
//! The Textual TUI wizard from the Python version is not ported.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::api::ApiHelper;
use crate::config::{Channel, Config, DEFAULT_SPONSORBLOCK_API, Device};
use crate::{dial, http_client};

const CATEGORIES: &str = "sponsor, selfpromo, exclusive_access, interaction, intro, outro, preview, filler, music_offtopic, hook";

fn input(prompt: &str) -> String {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    line.trim().to_string()
}

/// Returns Some(true/false), or None if the user just pressed enter.
fn yes_no(prompt: &str) -> Option<bool> {
    loop {
        match input(prompt).to_lowercase().as_str() {
            "" => return None,
            "y" => return Some(true),
            "n" => return Some(false),
            _ => println!("Invalid input. Please enter 'y' or 'n'."),
        }
    }
}

fn parse_categories(s: &str) -> Vec<String> {
    s.split([',', ' '])
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect()
}

fn add_device(config: &mut Config, screen_id: String, name: String) {
    if config.devices.iter().any(|d| d.screen_id == screen_id) {
        println!("{name} is already configured");
        return;
    }
    println!("Added device: {name}");
    config.devices.push(Device {
        screen_id,
        name,
        offset: 0.0,
        extra: Default::default(),
    });
}

pub async fn run(data_dir: &Path) -> Result<()> {
    let mut config = Config::load(data_dir)?;
    println!("Welcome to the iSponsorBlockTV (Rust) setup wizard");

    config.use_proxy = yes_no("Do you want to use system-wide proxy? (y/N) ").unwrap_or(false);
    if config.extra.contains_key("atvs")
        && yes_no("Remove the legacy 'atvs' entry (the app won't start with it present)? (y/N) ")
            == Some(true)
    {
        config.extra.remove("atvs");
    }

    let http = http_client(config.use_proxy)?;
    let api = Arc::new(ApiHelper::new(&config, http));

    if yes_no("Search for YouTube devices on your network? (y/N) ") == Some(true) {
        println!("Searching (this may briefly open YouTube on devices that need pairing)...");
        let found = dial::discover(api.clone(), true).await?;
        if found.is_empty() {
            println!("No devices found. You can pair with a TV code instead.");
        }
        for d in found {
            if yes_no(&format!("Found {}. Add it? (Y/n) ", d.name)) != Some(false) {
                add_device(&mut config, d.screen_id, d.name);
            }
        }
    }

    while yes_no(&format!(
        "{} device(s) configured. Pair one with a TV code? (y/N) ",
        config.devices.len()
    )) == Some(true)
    {
        let code = input("Enter pairing code (found in Settings - Link with TV code): ");
        println!("Pairing...");
        match api.pair_with_code(&code).await {
            Ok(Some(s)) => add_device(
                &mut config,
                s.screen_id,
                s.name.unwrap_or_else(|| "YouTube TV".into()),
            ),
            Ok(None) => println!("Failed to pair device. Try restarting the YouTube app"),
            Err(e) => println!("Failed to pair device: {e:#}"),
        }
    }

    let prompt = if config.apikey.is_empty() {
        "API key only needed for the channel whitelist function. Add it? (y/N) "
    } else {
        "API key already specified. Change it? (y/N) "
    };
    if yes_no(prompt) == Some(true) {
        println!(
            "Get a YouTube API key here: https://developers.google.com/youtube/registering_an_application"
        );
        config.apikey = input("Enter your API key: ");
    }

    let ask_categories = config.skip_categories.is_none()
        || yes_no("Skip categories already specified. Change them? (y/N) ") == Some(true);
    if ask_categories {
        let c = input(&format!(
            "Enter skip categories (space or comma separated) Options: [{CATEGORIES}]:\n"
        ));
        config.skip_categories = Some(parse_categories(&c));
    }

    if yes_no("Do you want to whitelist any channels from being ad-blocked? (y/N) ") == Some(true) {
        if config.apikey.is_empty() {
            println!(
                "WARNING: You need an API key to use this function, otherwise the program will fail to start."
            );
        }
        let api = ApiHelper::new(&config, http_client(config.use_proxy)?);
        loop {
            let q = input("Enter a channel name or \"/exit\" to exit: ");
            if q == "/exit" {
                break;
            }
            let results = match api.search_channels(&q).await {
                Ok(r) => r,
                Err(e) => {
                    println!("Search failed: {e:#}");
                    vec![]
                }
            };
            for (i, (_, title, subs)) in results.iter().enumerate() {
                println!("{i}: {title} - Subs: {subs}");
            }
            println!("5: Enter a custom channel ID\n6: Go back");
            let choice: usize = loop {
                match input("Select one option of the above [0-6]: ").parse::<usize>() {
                    Ok(n) if n == 5 || n == 6 || n < results.len() => break n,
                    _ => println!("Invalid choice"),
                }
            };
            let channel = match choice {
                5 => Channel {
                    id: input("Enter a channel ID: "),
                    name: input("Enter the channel name: "),
                    extra: Default::default(),
                },
                6 => continue,
                n => Channel {
                    id: results[n].0.clone(),
                    name: results[n].1.clone(),
                    extra: Default::default(),
                },
            };
            config.channel_whitelist.push(channel);
        }
    }

    if yes_no("Do you want to specify a minimum length of segment to skip? (y/N) ") == Some(true) {
        config.minimum_skip_length = loop {
            match input("Enter minimum length of segment to skip in seconds (enter 0 to disable): ")
                .parse()
            {
                Ok(n) => break n,
                Err(_) => println!("You entered a non-numeric value, try again."),
            }
        };
    }

    config.skip_count_tracking = yes_no(
        "Do you want to report skipped segments to sponsorblock. Only the segment UUID will be sent? (Y/n) ",
    ) != Some(false);
    config.mute_ads =
        yes_no("Do you want to mute native YouTube ads automatically? (y/N) ") == Some(true);
    config.skip_ads =
        yes_no("Do you want to skip native YouTube ads automatically? (y/N) ") == Some(true);
    config.auto_play = yes_no("Do you want to enable autoplay? (Y/n) ") != Some(false);

    let url = input(&format!(
        "Enter SponsorBlock API URL (default: {DEFAULT_SPONSORBLOCK_API}): "
    ));
    config.sponsorblock_api_url = if url.is_empty() {
        DEFAULT_SPONSORBLOCK_API.into()
    } else if url.ends_with('/') {
        url
    } else {
        format!("{url}/")
    };

    config.save(data_dir)?;
    println!("Config saved to {}", Config::path(data_dir).display());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn splits_categories() {
        assert_eq!(
            super::parse_categories("sponsor, intro  outro,"),
            vec!["sponsor", "intro", "outro"]
        );
    }
}
