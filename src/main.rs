use anyhow::{anyhow, bail, ensure, Context, Result};
use handlebars::Handlebars;
use regex::{CaptureLocations, Regex};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

static BRANCH: &str = "release/7";
static CONTAINER: &str = "ghcr.io/evalf/nutils:7";

macro_rules! run {
    (@head $prog:expr$(, $arg:expr)*) => {
        Command::new($prog)
            $(.arg($arg))*
            .stdin(Stdio::null())
    };
    (@tail $command:expr) => {
        $command.status()
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

#[derive(Deserialize)]
struct ExampleMetadata {
    name: String,
    authors: Vec<String>,
    description: String,
    repository: String,
    commit: String,
    script: String,
    tags: Vec<String>,
    images: Option<Vec<String>>,
    image_index: Option<usize>,
    thumbnail: Option<usize>,
}

impl ExampleMetadata {
    fn from_yaml(path: impl AsRef<Path>) -> Result<Self> {
        let metadata_file = BufReader::new(File::open(path)?);
        Ok(serde_yaml::from_reader(metadata_file)?)
    }
    fn from_py(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let output = Command::new("git")
            .current_dir(path.parent().context("script path has not parent")?)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .args(["rev-parse", "--show-toplevel", "HEAD"])
            .output()?;
        ensure!(
            output.status.success(),
            "Failed to get git hash of release branch"
        );
        let stdout = String::from_utf8(output.stdout)?;
        let lines: Vec<_> = stdout.lines().collect();
        ensure!(lines.len() == 2, "rev-parse yielded incorrect output");
        let script = path
            .strip_prefix(lines[0])?
            .to_str()
            .context("cannot convert osstr to str")?
            .to_owned();
        let commit = lines[1].to_owned();

        let code = fs::read_to_string(path)?;
        let mut code_lines = code.lines();

        let name = code_lines
            .next()
            .context("premature end of file")?
            .strip_prefix("# ")
            .context("expected comment")?
            .to_owned();
        ensure!(
            code_lines.next() == Some("#"),
            "second line should be an empty comment"
        );

        let mut description = String::new();
        for line in code_lines.by_ref().map_while(|l| l.strip_prefix('#')) {
            if let Some(text) = line.strip_prefix(' ') {
                description.push_str(text);
            } else {
                ensure!(line.len() == 0, "expected space or newline");
            }
            description.push('\n');
        }

        let mut thumbnail = None;
        let mut tags = vec!["official".to_string()];
        if let Some(modeline) = code_lines.last().and_then(|l| l.strip_prefix("# example:")) {
            for item in modeline.split(':') {
                match item.split_once('=') {
                    Some(("thumbnail", arg)) => {
                        thumbnail = Some(arg.parse()?);
                    }
                    Some(("tags", arg)) => {
                        tags.extend(arg.split(',').map(|tag| tag.to_owned()));
                    }
                    _ => {
                        bail!("invalid modeline");
                    }
                }
            }
        }
        Ok(Self {
            name,
            authors: vec!["Evalf".to_string(), "other Nutils contributors".to_string()],
            description,
            repository: "https://github.com/evalf/nutils.git".to_owned(),
            commit,
            script,
            tags,
            images: None,
            image_index: None,
            thumbnail,
        })
    }
    fn get_script_url(&self) -> Result<String> {
        let prefix = self
            .repository
            .strip_suffix(".git")
            .context("repository does not end with .git")?;
        if prefix.starts_with("https://github.com/") {
            Ok(format!("{}/blob/{}/{}", prefix, self.commit, self.script))
        } else if prefix.starts_with("https://gitlab.com/") {
            Ok(format!("{}/-/blob/{}/{}", prefix, self.commit, self.script))
        } else {
            Err(anyhow!(
                "don't know how to for script url for {}",
                self.repository
            ))
        }
    }
    fn run_script(&self) -> Result<PathBuf> {
        let app_dir_handle = tempfile::tempdir()?;
        let app_dir = app_dir_handle.path();
        run!("git", "init", "-b", "main"; cwd=&app_dir)?;
        run!("git", "fetch", "--depth", "1", &self.repository, &self.commit; cwd=&app_dir)?;
        run!("git", "-c", "advice.detachedHead=false", "checkout", "FETCH_HEAD"; cwd=&app_dir)?;
        if app_dir.join("nutils").is_dir() {
            // To prevent importing Nutils from the repository, rename the `nutils`
            // directory.
            fs::rename(app_dir.join("nutils"), app_dir.join("nutils.tmp"))?;
        }
        let log_dir = tempfile::tempdir_in(".")?;
        run!(
            "podman",
            "run",
            "--rm",
            "--network=none",
            os_str_join!["--mount=type=bind,destination=/app,source=", app_dir],
            os_str_join!["--mount=type=bind,destination=/log,source=", log_dir.path()],
            CONTAINER,
            &self.script
        )?;
        Ok(log_dir.into_path())
    }
    fn get_images(&self, log_file: &Path) -> Result<Vec<String>> {
        let mut image_sequences: Vec<(String, Vec<String>)> = self
            .images
            .as_ref()
            .unwrap_or(&Vec::new()) // would be nice to use uwrap_or_default here somehow
            .iter()
            .map(|name| (name.clone(), Vec::new()))
            .collect();
        for (name, filename) in ImageIterator::new(log_file)? {
            if let Some(item) = image_sequences.iter_mut().find(|item| item.0 == name) {
                item.1.push(filename);
            } else if self.images.is_none() {
                image_sequences.push((name, vec![filename]));
            }
        }
        Ok(image_sequences
            .into_iter()
            .map(|(_, filenames)| filenames.into_iter())
            .filter_map(|filenames| nth_or_last(filenames, self.image_index))
            .collect())
    }
}

struct ImageIterator {
    log: BufReader<File>,
    re: Regex,
    locs: CaptureLocations,
    linebuf: String,
}

impl ImageIterator {
    fn new(log_path: impl AsRef<Path>) -> Result<Self> {
        let log = BufReader::new(File::open(log_path)?);
        let re =
            Regex::new("<a href=\"([0-9a-f]{40}+.(?:png|jpg))\" download=\"(.+?)\">(.+?)</a>")?;
        let locs = re.capture_locations();
        Ok(ImageIterator {
            log,
            re,
            locs,
            linebuf: String::new(),
        })
    }
}

impl Iterator for ImageIterator {
    type Item = (String, String);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.linebuf.clear();
            let n = self.log.read_line(&mut self.linebuf).unwrap();
            if n == 0 {
                break None;
            }
            if self
                .re
                .captures_read(&mut self.locs, &self.linebuf)
                .is_some()
            {
                let (n1, m1) = self.locs.get(1).unwrap();
                let (n2, m2) = self.locs.get(2).unwrap();
                break Some((
                    self.linebuf[n2..m2].to_string(),
                    self.linebuf[n1..m1].to_string(),
                ));
            }
        }
    }
}

fn render_markdown(markdown: &str) -> String {
    use pulldown_cmark::html::push_html;
    use pulldown_cmark::Parser;
    let mut html = String::with_capacity(markdown.len() * 2 / 3);
    let parser = Parser::new(markdown);
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

fn nth_or_last<T, I: Iterator<Item = T>>(mut items: I, n: Option<usize>) -> Option<T> {
    if let Some(n) = n {
        items.nth(n)
    } else {
        items.last()
    }
}

fn file_stem(path: &Path) -> Result<&str> {
    path.file_stem()
        .context("cannot extract file stem")?
        .to_str()
        .context("cannot convert osstr to str")
}

fn copy_all(source_dir: impl AsRef<Path>, destination_dir: impl AsRef<Path>) -> Result<()> {
    for item in fs::read_dir(source_dir)? {
        let item = item?;
        if item.file_type()?.is_file() {
            fs::copy(item.path(), destination_dir.as_ref().join(item.file_name()))?;
        }
    }
    Ok(())
}

fn examples() -> Result<BTreeMap<String, ExampleMetadata>> {
    let git_dir_handle = tempfile::tempdir()?;
    let git_dir = git_dir_handle.path();
    run!("git", "clone", "--branch", BRANCH, "--depth", "1", "https://github.com/evalf/nutils.git", "."; cwd=git_dir)?;
    let mut examples = BTreeMap::new();
    for entry in git_dir.join("examples").read_dir()? {
        let path = entry?.path();
        let stem = file_stem(&path)?;
        if stem != "__init__" && path.extension() == Some(OsStr::new("py")) {
            examples.insert(
                format!("official-{stem}"),
                ExampleMetadata::from_py(&path)
                    .with_context(|| format!("Failed to generate metadata from {stem}.py"))?,
            );
        }
    }
    for entry in Path::new("examples").read_dir()? {
        let path = entry?.path();
        let stem = file_stem(&path)?;
        if path.extension() == Some(OsStr::new("yaml")) {
            examples.insert(
                format!("user-{stem}"),
                ExampleMetadata::from_yaml(&path)
                    .with_context(|| format!("Failed to generate metadata from {stem}.yaml"))?,
            );
        }
    }
    Ok(examples)
}

fn main() -> Result<()> {
    let target = Path::new("target/website");
    fs::create_dir_all(target).context("Failed to create output directory")?;
    copy_all("static", target).context("Failed to copy static data")?;

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

    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars
        .register_template_file("example", "templates/example.hbs")
        .context("Failed to load example template")?;
    handlebars
        .register_template_file("examples-list", "templates/examples-list.hbs")
        .context("Failed to load example-list template")?;

    let mut examples_list = Vec::new();

    for (id, metadata) in examples()? {
        println!("generating html for {id}");

        let log_dir = target.join(&id);
        if !log_dir.is_dir() {
            fs::rename(
                metadata.run_script().context("Failed to run script")?,
                &log_dir,
            )
            .context("Failed to rename output directory")?;
        }

        let images = metadata
            .get_images(&log_dir.join("log.html"))
            .context("Failed to find images")?;

        examples_list.push(ExampleListContext {
            name: metadata.name.to_string(),
            thumbnail: nth_or_last(images.iter(), metadata.thumbnail)
                .map(|image| format!("{id}/{image}")),
            tags: metadata.tags.to_vec(),
            href: format!("{id}/"),
        });

        let context = ExampleContext {
            name: metadata.name.to_string(),
            authors: comma_and_join(&metadata.authors),
            description: render_markdown(&metadata.description),
            images,
            tags: metadata.tags.to_vec(),
            script: metadata.script.to_string(),
            repository: metadata.repository.to_string(),
            commit: metadata.commit.to_string(),
            script_url: metadata
                .get_script_url()
                .context("Failed to generate url for git repository")?,
        };

        handlebars
            .render_to_write(
                "example",
                &context,
                BufWriter::new(
                    File::create(log_dir.join("index.html"))
                        .context("Failed to open example page for writing")?,
                ),
            )
            .context("Failed to render example page")?;
    }

    handlebars
        .render_to_write(
            "examples-list",
            &examples_list,
            BufWriter::new(
                File::create(target.join("index.html"))
                    .context("Failed to open overview page for writing")?,
            ),
        )
        .context("Failed to render example page")?;

    Ok(())
}
