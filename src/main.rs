mod summarize;
mod transcribe;

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use aws_config::meta::region::RegionProviderChain;
use aws_config::{Region, SdkConfig};
use aws_sdk_s3::config::StalledStreamProtectionConfig;
use clap::Parser;
use config::{Config, File as ConfigFile};
use docx_rs::{Docx, Paragraph, Run};
use reqwest::Client as ReqwestClient;
use serde_json::json;
use spinoff::{spinners, Color, Spinner};

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use dialoguer::{theme::ColorfulTheme, Select};

#[derive(Debug, Parser)]
#[clap(
    about = "Distill CLI can summarize an audio file (e.g., a meeting) using Amazon Transcribe and Amazon Bedrock.",
    after_help = "For supported languages, consult: https://docs.aws.amazon.com/transcribe/latest/dg/supported-languages.html"
)]
struct Opt {
    #[clap(short, long)]
    input_audio_file: String,

    #[clap(
        short,
        long,
        value_enum,
        ignore_case = true
    )]
    output_type: Option<OutputType>,

    #[clap(long, help = "Specify the output filename (only valid with text, word, or markdown output types)")]
    output_filename: Option<String>,

    #[clap(short, long, default_value = "en-US")]
    language_code: String,

    #[clap(short, long, default_value = "n")]
    delete_s3_object: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputType {
    Terminal,
    Text,
    Word,
    Markdown,
    Slack,
}

impl std::fmt::Display for OutputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputType::Terminal => write!(f, "terminal"),
            OutputType::Text => write!(f, "text"),
            OutputType::Word => write!(f, "word"),
            OutputType::Markdown => write!(f, "markdown"),
            OutputType::Slack => write!(f, "slack"),
        }
    }
}

impl OutputType {
    fn from_filename(filename: &str) -> Option<Self> {
        let extension = std::path::Path::new(filename)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s.to_lowercase());
        
        match extension.as_deref() {
            Some("md") => Some(OutputType::Markdown),
            Some("txt") => Some(OutputType::Text),
            Some("doc" | "docx") => Some(OutputType::Word),
            _ => None
        }
    }
}

#[::tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let config = load_config(None).await;

    let settings = Config::builder()
        .add_source(ConfigFile::with_name("./config.toml"))
        .build()?;

    let s3_bucket_name = settings
        .get_string("aws.s3_bucket_name")
        .unwrap_or_default();

    let Opt {
        input_audio_file,
        output_type,
        output_filename,
        language_code,
        delete_s3_object,
    } = Opt::parse();

    // Handle output type inference and validation
    let actual_output_type = match (&output_filename, output_type) {
        (Some(filename), None) => {
            // Try to infer from filename if type not explicitly specified
            OutputType::from_filename(filename).unwrap_or_else(|| {
                println!("Warning: Could not infer output type from filename '{}', defaulting to text", filename);
                OutputType::Text
            })
        },
        (Some(filename), Some(explicit_type)) => {
            match (filename, explicit_type) {
                (_, OutputType::Terminal) => bail!("Output filename cannot be used with terminal output type"),
                (_, OutputType::Slack) => bail!("Output filename cannot be used with Slack output type"),
                (_, _) => {}
            }
        
            if let Some(inferred_type) = OutputType::from_filename(filename) {
                if explicit_type != inferred_type {
                    println!("Warning: Output filename extension suggests {} output type, but {} was explicitly specified",
                        inferred_type,
                        explicit_type);
                }
            }
            explicit_type
        },
        (None, Some(t)) => t,
        (None, None) => OutputType::Terminal,
    };

    let s3_client = Client::new(&config);

    let mut bucket_name = String::new();

    println!("🧙 Welcome to Distill CLI");

    let resp = &list_buckets(&s3_client).await;

    if !s3_bucket_name.is_empty() {
        if resp
            .as_ref()
            .ok()
            .and_then(|buckets| buckets.iter().find(|b| b.as_str() == s3_bucket_name))
            .is_some()
        {
            println!("📦 S3 bucket name: {}", s3_bucket_name);
            bucket_name = s3_bucket_name;
        } else {
            println!(
                "Error: The configured S3 bucket '{}' was not found.",
                s3_bucket_name
            );
        }
    }

    if bucket_name.is_empty() {
        match resp {
            Ok(bucket_names) => {
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Choose a destination S3 bucket for your audio file")
                    .default(0)
                    .items(&bucket_names[..])
                    .interact()?;

                bucket_name.clone_from(&bucket_names[selection]);
            }
            Err(err) => {
                println!("Error getting bucket list: {}", err);
                bail!("\nError getting bucket list: {}", err);
            }
        };
    }

    if bucket_name.is_empty() {
        bail!("\nNo valid S3 bucket found. Please check your AWS configuration.");
    }

    let mut spinner = Spinner::new(spinners::Dots7, "Uploading file to S3...", Color::Green);

    // Load the bucket region and create a new client to use that region
    let region = bucket_region(&s3_client, &bucket_name).await?;
    println!();
    spinner.update(
        spinners::Dots7,
        format!("Using bucket region {}", region),
        None,
    );
    let regional_config = load_config(Some(region)).await;
    let regional_s3_client = Client::new(&regional_config);

    // Handle conversion of relative paths to absolute paths
    let file_path = Path::new(&input_audio_file);
    let file_name = file_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let absolute_path = shellexpand::tilde(file_path.to_str().unwrap()).to_string();
    let absolute_path = Path::new(&absolute_path);

    if !absolute_path.exists() {
        bail!("\nThe path {} does not exist.", absolute_path.display());
    }

    let canonicalized_path = absolute_path.canonicalize()?;
    let body = ByteStream::from_path(&canonicalized_path)
        .await
        .with_context(|| format!("Error loading file: {}", canonicalized_path.display()))?;

    let _upload_result = regional_s3_client
        .put_object()
        .bucket(&bucket_name)
        .key(&file_name)
        .body(body)
        .send()
        .await
        .context("Failed to upload to S3")?;

    let s3_uri = format!("s3://{}/{}", bucket_name, file_name);

    println!();
    spinner.update(spinners::Dots7, "Summarizing text...", None);

    // Transcribe the audio
    let transcription: String = transcribe::transcribe_audio(
        &regional_config,
        file_path,
        &s3_uri,
        &mut spinner,
        &language_code,
    )
    .await?;

    // Summarize the transcription
    spinner.update(spinners::Dots7, "Summarizing text...", None);
    let summarized_text = summarize::summarize_text(&config, &transcription, &mut spinner).await?;

    match actual_output_type {
        OutputType::Word => {
            let filename = match &output_filename {
                Some(f) => f,
                None => "summary.docx",
            };
            let file = File::create(filename)
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;

            // Creating a new document and adding paragraphs
            let doc = Docx::new()
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text(&summarized_text)))
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text("\n\n")))
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Transcription:\n")))
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text(&transcription)));

            // Building and saving the document
            doc.build()
                .pack(file)
                .map_err(|e| anyhow::anyhow!("Error writing Word document: {}", e))?;

            spinner.success("Done!");
            println!(
                "💾 Summary and transcription written to {}",
                filename
            );
        }
        OutputType::Text => {
            let filename = match &output_filename {
                Some(f) => f,
                None => "summary.txt",
            };
            let mut file = File::create(filename)
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;

            file.write_all(summarized_text.as_bytes())
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;
            file.write_all(b"\n\nTranscription:\n")
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;
            file.write_all(transcription.as_bytes())
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;

            spinner.success("Done!");
            println!(
                "💾 Summary and transcription written to {}",
                filename
            );
        }
        OutputType::Terminal => {
            spinner.success("Done!");
            println!();
            println!("Summary:\n{}\n", summarized_text);
            println!("Transcription:\n{}\n", transcription);
        }
        OutputType::Markdown => {
            let filename = match &output_filename {
                Some(f) => f,
                None => "summary.md",
            };
            let mut file = File::create(filename)
                .map_err(|e| anyhow::anyhow!("Error creating file: {}", e))?;

            let summary_md = format!("# Summary\n\n{}", summarized_text);
            let mut transcription_md = format!("\n\n# Transcription\n\n{}", transcription);
            transcription_md = transcription_md.replace("spk_", "\nspk_");
            let markdown_content = format!("{}{}", summary_md, transcription_md);

            file.write_all(markdown_content.as_bytes())
                .map_err(|e| anyhow::anyhow!("Error writing Markdown file: {}", e))?;

            spinner.success("Done!");
            println!(
                "💾 Summary and transcription written to {}",
                filename
            );
        }
        OutputType::Slack => {
            let client = ReqwestClient::new();

            let slack_webhook_endpoint = settings
                .get_string("slack.webhook_endpoint")
                .unwrap_or_default();

            if slack_webhook_endpoint.is_empty() {
                spinner.stop_and_persist(
                    "⚠️",
                    "Slack webhook endpoint is not configured. Skipping Slack notification.",
                );
                println!("Summary:\n{}\n", summarized_text);
            } else {
                let (summary, action_items, rest) = parse_summary_sections(&summarized_text);
                let _content = format!("A summarization job just completed:\n\n{}\n{}", input_audio_file, summarized_text);
                let payload = json!({
                    "Content": input_audio_file,
                    "SummaryText": summary,
                    "KeyActions": action_items,
                    "Others": rest
                });
                match client
                    .post(slack_webhook_endpoint)
                    .header("Content-Type", "application/json")
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(response) => {
                        if response.status().is_success() {
                            spinner.success("Summary sent to Slack!");
                        } else {
                            spinner.stop_and_persist("❌", "Failed to send summary to Slack!");
                            eprintln!("Error sending summary to Slack: {}", response.status());
                        }
                    }
                    Err(err) => {
                        spinner.stop_and_persist("❌", "Failed to send summary to Slack!");
                        eprintln!("Error sending summary to Slack: {}", err);
                    }
                };
            }
        }
    }

    // After processing, check if the user wants to delete the S3 object
    if delete_s3_object == "Y" {
        s3_client
            .delete_object()
            .bucket(&bucket_name)
            .key(&file_name)
            .send()
            .await?;
    }

    Ok(())
}

// Load the user's aws config, default region to us-east-1 if none is provided or can be found
async fn load_config(region: Option<Region>) -> SdkConfig {
    let mut config = aws_config::from_env();
    match region {
        Some(region) => config = config.region(region),
        None => {
            config = config.region(RegionProviderChain::default_provider().or_else("us-east-1"))
        }
    }

    // Resolves issues with uploading large S3 files
    // See https://github.com/awslabs/aws-sdk-rust/issues/1146
    config = config
        .stalled_stream_protection(
            StalledStreamProtectionConfig::disabled()
        );

    config.load().await
}

fn parse_summary_sections(summarized_text: &str) -> (String, String, String) {
    // Initialize empty sections
    let mut summary = String::new();
    let mut action_items = String::new();
    let mut rest = String::new();

    // Split text by lines for processing
    let lines: Vec<&str> = summarized_text.lines().collect();
    let mut current_section = "";
    
    for line in lines {
        // Check for section headers
        if line.to_lowercase().contains("key points") || 
           line.to_lowercase().contains("summary") {
            current_section = "summary";
            continue;
        } else if line.to_lowercase().contains("action item") || 
                  line.to_lowercase().contains("next step") {
            current_section = "action";
            continue;
        } else if line.trim().is_empty() {
            continue;
        }

        // Append content to appropriate section
        match current_section {
            "summary" => summary.push_str(&format!("{}\n", line)),
            "action" => action_items.push_str(&format!("{}\n", line)),
            _ => rest.push_str(&format!("{}\n", line)),
        }
    }

    // Trim whitespace from all sections
    (
        summary.trim().to_string(),
        action_items.trim().to_string(),
        rest.trim().to_string()
    )
}

async fn list_buckets(client: &Client) -> Result<Vec<String>> {
    let resp = client.list_buckets().send().await?;
    let buckets = resp.buckets();

    let bucket_names: Vec<String> = buckets
        .iter()
        .map(|bucket| bucket.name().unwrap_or_default().to_string())
        .collect();

    Ok(bucket_names)
}

async fn bucket_region(client: &Client, bucket_name: &str) -> Result<Region> {
    let resp = client
        .get_bucket_location()
        .bucket(bucket_name)
        .send()
        .await?;

    let location_constraint = resp
        .location_constraint()
        .context("Bucket has no location_constraint")?;

    if location_constraint.as_str() == "" {
        Ok(Region::new("us-east-1"))
    } else {
        Ok(Region::new(location_constraint.as_str().to_owned()))
    }
}
