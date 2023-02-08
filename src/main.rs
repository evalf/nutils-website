use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Deserialize)]
struct OfficialExampleMetadata {
    name: String,
    description: String,
    script: String,
    tags: Vec<String>,
    images: Vec<String>,
}

#[derive(Deserialize)]
struct ExampleMetadata {
    name: String,
    authors: Vec<String>,
    description: String,
    repository: String,
    commit: String,
    script: String,
    tags: Vec<String>,
    images: Vec<String>,
}

#[derive(Serialize, Deserialize)]
enum ExampleStatus {
    RunsOn { stable: bool, master: bool },
    FetchFailed,
}

#[derive(Debug)]
pub struct RunError;

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunError")
    }
}

impl std::error::Error for RunError {}

macro_rules! run {
    (@head $prog:expr$(, $arg:expr)*) => {
        Command::new($prog)
            $(.arg($arg))*
            .stdin(Stdio::null())
    };
    (@tail $command:expr) => {
        if $command.status().unwrap().success() {
            Ok(())
        } else {
            Err(RunError)
        }
    };
    ($prog:expr$(, $arg:expr)*; cwd=$cwd:expr) => {
        run!(@tail run!(@head $prog$(, $arg)*).current_dir($cwd))
    };
    ($prog:expr$(, $arg:expr)*) => {
        run!(@tail run!(@head $prog$(, $arg)*))
    };
}

macro_rules! os_str_join {
    ($($arg:expr),+) => {{
        let mut s = OsString::new();
        $(s.push($arg);)+
        s
    }};
}

fn run_script_in_container(
    metadata: &ExampleMetadata,
    image: &str,
    git_dir: impl AsRef<Path>,
    log_dir: impl AsRef<Path>,
) -> bool {
    let app_dir_handle = tempfile::tempdir().expect("failed to create temporary directory");
    let app_dir = app_dir_handle.path();
    run!("git", "worktree", "add", app_dir, "FETCH_HEAD"; cwd=git_dir)
        .expect("failed to checkout fetched branch");
    if app_dir.join("nutils").is_dir() {
        // To prevent importing Nutils from the repository, rename the `nutils`
        // directory.
        fs::rename(app_dir.join("nutils"), app_dir.join("nutils.tmp"))
            .expect("failed to move `nutils` directory to `nutils.tmp`");
    }
    run!(
        "podman",
        "run",
        "--rm",
        "--network=none",
        os_str_join!["--mount=type=bind,destination=/app,source=", app_dir],
        os_str_join![
            "--mount=type=bind,destination=/log,source=",
            log_dir.as_ref()
        ],
        format!("ghcr.io/evalf/nutils:{}", image),
        &metadata.script
    )
    .is_ok()
}

fn update_examples() {
    // Initialize a single git repository that will be used to fetch all
    // examples. When running an example a specific branch will be checked out in
    // a disposable worktree.
    let git_dir_handle = tempfile::tempdir().expect("failed to create temporary directory");
    let git_dir = git_dir_handle.path();
    run!("git", "init", "--bare"; cwd=git_dir).expect("failed to initialize git repository");

    let statuses: HashMap<String, ExampleStatus> = examples()
    .map(|(id, metadata)| {
      let log_dir = Path::new("target/website").join(&id);
      fs::create_dir_all(&log_dir).unwrap();
      remove_file_if_exists(log_dir.join("log.html")).unwrap();
      remove_file_if_exists(log_dir.join("stable.html")).unwrap();
      remove_file_if_exists(log_dir.join("master.html")).unwrap();

      if let Err(RunError) =
        run!("git", "fetch", "--depth", "1", &metadata.repository, &metadata.commit; cwd=&git_dir)
      {
        return (id, ExampleStatus::FetchFailed);
      }
      let stable = run_script_in_container(&metadata, "7", &git_dir, &log_dir);
      rename_if_exists(log_dir.join("log.html"), log_dir.join("stable.html")).unwrap();
      let master = run_script_in_container(&metadata, "latest", &git_dir, &log_dir);
      rename_if_exists(log_dir.join("log.html"), log_dir.join("master.html")).unwrap();

      (id, ExampleStatus::RunsOn { stable, master })
    })
    .collect();
    let writer = BufWriter::new(File::create("target/examples-statuses.json").unwrap());
    serde_json::to_writer_pretty(writer, &statuses).expect("failed to write examples statuses");
}

fn render_markdown(markdown: &str) -> String {
    use pulldown_cmark::html::push_html;
    use pulldown_cmark::Parser;
    let mut html = String::with_capacity(markdown.len() * 2 / 3);
    let parser = Parser::new(&markdown);
    push_html(&mut html, parser);
    html
}

fn comma_and_join(list: &Vec<String>) -> String {
    let mut joined = String::new();
    for (i, item) in list.iter().enumerate() {
        if i > 0 {
            joined.push_str(", ");
            if i == list.len() - 1 {
                joined.push_str(" and ");
            }
        }
        joined.push_str(item);
    }
    joined
}

fn remove_file_if_exists(path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

fn rename_if_exists(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    let src = src.as_ref();
    if src.exists() {
        fs::rename(src, dst)
    } else {
        Ok(())
    }
}

fn get_last_image_by_name(log_path: impl AsRef<Path>, name: &str) -> Option<String> {
    let log_path = log_path.as_ref();
    if !log_path.exists() {
        return None;
    }
    let re = Regex::new(
        "^<div class=\"item\" data-loglevel=\"2\">\
        <a href=\"([0-9a-f]+.(?:png|jpg))\" download=\"[^\"<>]*\">\
          ([^\"<>]*)\
        </a>\
      </div>$",
    )
    .unwrap();
    let mut file_name: Option<String> = None;
    for line in BufReader::new(File::open(log_path).unwrap()).lines() {
        if let Some(captures) = re.captures(&line.unwrap()) {
            if &captures[2] == name {
                file_name = Some(captures[1].to_string());
            }
        }
    }
    file_name
}

fn build_website() {
    use handlebars::Handlebars;

    #[derive(Serialize)]
    struct ExampleContext {
        name: String,
        authors: String,
        description: String,
        images: Vec<String>,
        repository: String,
        commit: String,
        script: String,
        script_url: String,
        tags: Vec<String>,
    }

    #[derive(Serialize)]
    struct ExampleListContext {
        name: String,
        thumbnail: Option<String>,
        tags: Vec<String>,
        href: String,
    }

    let re_github = Regex::new(r"^https://github\.com/([^/]+/[^/]+).git$").unwrap();

    let reader = BufReader::new(File::open("target/examples-statuses.json").unwrap());
    let statuses: HashMap<String, ExampleStatus> =
        serde_json::from_reader(reader).expect("failed to read examples statuses");
    fs::create_dir_all("target/website").unwrap();

    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars
        .register_template_file("example", "templates/example.hbs")
        .unwrap();
    handlebars
        .register_template_file("examples-list", "templates/examples-list.hbs")
        .unwrap();

    let mut examples_list = Vec::new();

    for (id, metadata) in examples() {
        let dir = Path::new("target/website").join(&id);
        let log = dir.join("stable.html");
        let images: Vec<String> = metadata
            .images
            .iter()
            .filter_map(|name| {
                ["stable.html", "master.html"]
                    .iter()
                    .filter_map(|log| get_last_image_by_name(&dir.join(log), name))
                    .next()
            })
            .collect();

        examples_list.push(ExampleListContext {
            name: metadata.name.to_string(),
            thumbnail: if let Some(image) = images.iter().last() {
                Some(format!("{}/{}", id, image))
            } else {
                None
            },
            tags: metadata.tags.to_vec(),
            href: format!("{}/", id),
        });

        let script_url = if let Some(cap) = re_github.captures(&metadata.repository) {
            format!(
                "https://github.com/{}/blob/{}/{}",
                &cap[1], metadata.commit, metadata.script
            )
        } else {
            panic!("unsupported repository");
        };

        let context = ExampleContext {
            name: metadata.name.to_string(),
            authors: comma_and_join(&metadata.authors),
            description: render_markdown(&metadata.description),
            images: images,
            tags: metadata.tags.to_vec(),
            script: metadata.script.to_string(),
            repository: metadata.repository.to_string(),
            commit: metadata.commit.to_string(),
            script_url,
        };

        let example_writer = BufWriter::new(File::create(dir.join("index.html")).unwrap());
        handlebars
            .render_to_write("example", &context, example_writer)
            .unwrap();
    }

    examples_list.sort_by_cached_key(|item| item.name.to_string());

    let examples_list_writer = BufWriter::new(File::create("target/website/index.html").unwrap());
    handlebars
        .render_to_write("examples-list", &examples_list, examples_list_writer)
        .unwrap();

    let target = Path::new("target/website");
    for entry in fs::read_dir("static").expect("failed to list the contents of dir 'static'") {
        let entry = entry.expect("failed to list the contents of dir 'static'");
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), target.join(entry.file_name()))
                .expect("failed to copy {entry:?}");
        }
    }
}

fn examples() -> impl Iterator<Item = (String, ExampleMetadata)> {
    let official = Path::new("examples/official")
        .read_dir()
        .expect("failed to iterate examples directory")
        .map(|entry| entry.expect("failed to read directory entry").path())
        .filter(|path| path.extension() == Some(OsStr::new("yaml")))
        .map(|path| {
            let id: String = path
                .file_stem()
                .expect("failed to extract file stem")
                .to_str()
                .expect("cannot convert file stem to str")
                .to_string();
            let metadata_file =
                BufReader::new(File::open(path).expect("failed to open example metadata"));
            let OfficialExampleMetadata {
                name,
                description,
                script,
                mut tags,
                images,
            } = serde_yaml::from_reader(metadata_file).expect("failed to parse example metadata");
            tags.push("official".to_string());
            let authors = vec!["Evalf".to_string(), "other Nutils contributors".to_string()];
            let repository = "https://github.com/evalf/nutils.git".to_owned();
            let metadata = ExampleMetadata {
                name,
                authors,
                description,
                repository,
                commit: "release/7".to_owned(),
                script,
                tags,
                images,
            };
            (format!("official-{}", id), metadata)
        });
    let user = Path::new("examples/user")
        .read_dir()
        .expect("failed to iterate examples directory")
        .map(|entry| entry.expect("failed to read directory entry").path())
        .filter(|path| path.extension() == Some(OsStr::new("yaml")))
        .map(|path| {
            let id: String = path
                .file_stem()
                .expect("failed to extract file stem")
                .to_str()
                .expect("cannot convert file stem to str")
                .to_string();
            let metadata_file =
                BufReader::new(File::open(path).expect("failed to open example metadata"));
            let metadata: ExampleMetadata =
                serde_yaml::from_reader(metadata_file).expect("failed to parse example metadata");
            (format!("user-{}", id), metadata)
        });
    official.chain(user)
}

fn main() {
    update_examples();
    build_website();
}
