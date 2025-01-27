use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pulldown_cmark::{html, Options, Parser as MarkdownParser};
use rss::{ChannelBuilder, ItemBuilder};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    sync::mpsc::channel,
    time::Duration,
};
use tera::{Context as TeraContext, Tera};
use walkdir::WalkDir;
use chrono::{DateTime, Utc, Datelike};
use notify::{Watcher, RecursiveMode};
use tokio;
use warp::Filter;

#[derive(Parser, Clone)]
#[command(name = "trk")]
#[command(about = "trkw's simple static site generator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, default_value = "content")]
    content_dir: String,
    
    #[arg(short, long, default_value = "templates")]
    template_dir: String,
    
    #[arg(short, long, default_value = "public")]
    output_dir: String,
}

#[derive(Subcommand, Clone)]
enum Commands {
    /// Generate static site
    Generate,
    /// Start development server with hot reload
    Dev {
        #[arg(short, long, default_value = "3000")]
        port: u16,
    },
}

#[derive(Deserialize, Serialize)]
struct FrontMatter {
    title: String,
    date: DateTime<Utc>,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
struct Post {
    front_matter: FrontMatter,
    content: String,
    slug: String,
    formatted_date: String,
}

fn format_date(date: &DateTime<Utc>) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

fn parse_markdown_file(path: &Path) -> Result<Post> {
    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.split("---\n").collect();
    
    if parts.len() < 3 {
        anyhow::bail!("Invalid markdown file format: {}", path.display());
    }
    
    let front_matter: FrontMatter = serde_yaml::from_str(parts[1])?;
    let markdown_content = parts[2];
    
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    
    let parser = MarkdownParser::new_ext(markdown_content, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

    let slug = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    
    let formatted_date = format_date(&front_matter.date);
    let post = Post {
        front_matter,
        content: html_output,
        slug,
        formatted_date,
    };
    Ok(post)
}

fn generate_rss(posts: &[Post], output_dir: &Path) -> Result<()> {
    let mut channel = ChannelBuilder::default()
        .title("Memo")
        .link("https://trkw.github.io")
        .description("My memo posts")
        .build();
        
    for post in posts {
        let item = ItemBuilder::default()
            .title(post.front_matter.title.clone())
            .link(format!("https://trkw.github.io/{}.html", post.slug))
            .description(post.front_matter.description.clone())
            .pub_date(post.front_matter.date.to_rfc2822())
            .build();
            
        channel.items.push(item);
    }
    
    let rss_path = output_dir.join("feed.xml");
    let mut file = File::create(rss_path)?;
    file.write_all(channel.to_string().as_bytes())?;
    
    Ok(())
}

fn generate_site(cli: &Cli) -> Result<()> {
    // Create output directory if it doesn't exist
    fs::create_dir_all(&cli.output_dir)?;
    
    // Initialize Tera template engine
    let tera = Tera::new(&format!("{}/**/*.html", cli.template_dir))
        .context("Failed to initialize template engine")?;
    
    // Collect all markdown files
    let mut posts = Vec::new();
    for entry in WalkDir::new(&cli.content_dir) {
        let entry = entry?;
        if entry.path().extension().map_or(false, |ext| ext == "md") {
            let post = parse_markdown_file(entry.path())?;
            posts.push(post);
        }
    }
    
    // Sort posts by date
    posts.sort_by(|a, b| b.front_matter.date.cmp(&a.front_matter.date));
    
    // Generate individual post pages
    for post in &posts {
        let mut context = TeraContext::new();
        context.insert("title", &post.front_matter.title);
        context.insert("content", &post.content);
        context.insert("date", &post.formatted_date);
        
        let output = tera.render("post.html", &context)?;
        let output_path = Path::new(&cli.output_dir).join(format!("{}.html", post.slug));
        fs::write(output_path, output)?;
    }
    
    // Generate index page
    let mut context = TeraContext::new();
    context.insert("posts", &posts);
    let output = tera.render("index.html", &context)?;
    fs::write(Path::new(&cli.output_dir).join("index.html"), output)?;
    
    // Generate RSS feed
    generate_rss(&posts, Path::new(&cli.output_dir))?;
    
    println!("Site generated successfully!");
    Ok(())
}

async fn serve_static_files(output_dir: String) {
    let dir = output_dir.clone();
    let static_files = warp::fs::dir(dir);
    let routes = static_files.with(warp::log("trk::dev"));
    
    println!("Starting development server at http://localhost:3000");
    warp::serve(routes).run(([127, 0, 0, 1], 3000)).await;
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Generate => {
            generate_site(&cli)?;
        }
        Commands::Dev { port: _ } => {
            // Initial generation
            generate_site(&cli)?;

            // Setup file watcher
            let (tx, rx) = channel();
            let mut watcher = notify::recommended_watcher(tx)?;

            // Watch content and template directories
            watcher.watch(Path::new(&cli.content_dir), RecursiveMode::Recursive)?;
            watcher.watch(Path::new(&cli.template_dir), RecursiveMode::Recursive)?;

            // Clone values for async block
            let cli_clone = cli.clone();

            // Spawn file watcher handler
            tokio::spawn(async move {
                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(_) => {
                            println!("Changes detected, regenerating site...");
                            if let Err(e) = generate_site(&cli_clone) {
                                eprintln!("Error regenerating site: {}", e);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(e) => {
                            eprintln!("Watch error: {}", e);
                            break;
                        }
                    }
                }
            });

            // Start static file server
            serve_static_files(cli.output_dir).await;
        }
    }

    Ok(())
}

