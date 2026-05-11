// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub struct GitFixture {
    path: PathBuf,
    private_home: PathBuf,
    global_config: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIdentity {
    name: String,
    email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOptions {
    message: String,
    author: GitIdentity,
    committer: GitIdentity,
    author_date: String,
    committer_date: String,
}

impl GitFixture {
    pub fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::current_dir()
            .expect("test should have a current directory")
            .join("target")
            .join("git-fixtures")
            .join(format!("{name}-{}-{id}", std::process::id()));
        let private_home = path.join(".git-fixture-home");
        let global_config = private_home.join(".gitconfig");

        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&private_home).expect("fixture private home should be created");

        let fixture = Self {
            path,
            private_home,
            global_config,
        };

        fixture.git_ok(["init", "--quiet"]);
        fixture.git_ok(["checkout", "--quiet", "-b", "main"]);
        fixture.git_ok(["config", "user.name", "Hotpath Fixture"]);
        fixture.git_ok(["config", "user.email", "hotpath.fixture@example.invalid"]);
        fixture.git_ok(["config", "commit.gpgsign", "false"]);
        fixture.git_ok(["config", "core.autocrlf", "false"]);
        fixture.git_ok(["config", "core.eol", "lf"]);
        fixture.git_ok(["config", "core.quotePath", "false"]);
        fixture.git_ok(["config", "gc.auto", "0"]);

        fixture
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, relative_path: impl AsRef<Path>, contents: &str) {
        let path = self.fixture_path(relative_path.as_ref());

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent should be created");
        }

        fs::write(path, contents).expect("fixture file should be written");
    }

    pub fn delete(&self, relative_path: impl AsRef<Path>) {
        fs::remove_file(self.fixture_path(relative_path.as_ref()))
            .expect("fixture file should be deleted");
    }

    pub fn commit(&self, options: CommitOptions) -> String {
        self.git_ok(["add", "--all"]);
        self.git_ok_with_commit_env(
            ["commit", "--quiet", "--message", options.message.as_str()],
            &options,
        );

        self.git_stdout(["rev-parse", "HEAD"]).trim().to_owned()
    }

    pub fn git_stdout<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git(args);

        assert!(
            output.status.success(),
            "git command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).expect("git stdout should be UTF-8")
    }

    fn fixture_path(&self, relative_path: &Path) -> PathBuf {
        assert!(
            is_safe_relative_path(relative_path),
            "fixture paths must be relative repository paths: {}",
            relative_path.display()
        );

        self.path.join(relative_path)
    }

    fn git_ok<I, S>(&self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git(args);

        assert!(
            output.status.success(),
            "git command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_ok_with_commit_env<I, S>(&self, args: I, options: &CommitOptions)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.git_command(args);

        command
            .env("GIT_AUTHOR_NAME", &options.author.name)
            .env("GIT_AUTHOR_EMAIL", &options.author.email)
            .env("GIT_AUTHOR_DATE", &options.author_date)
            .env("GIT_COMMITTER_NAME", &options.committer.name)
            .env("GIT_COMMITTER_EMAIL", &options.committer.email)
            .env("GIT_COMMITTER_DATE", &options.committer_date);

        let output = command.output().expect("git command should run");

        assert!(
            output.status.success(),
            "git command failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.git_command(args)
            .output()
            .expect("git command should run")
    }

    fn git_command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");

        command
            .args(args)
            .current_dir(&self.path)
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.private_home)
            .env("USERPROFILE", &self.private_home)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_AUTHOR_DATE")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("GIT_COMMITTER_DATE");

        command
    }
}

impl Drop for GitFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl GitIdentity {
    pub fn new(name: &str, email: &str) -> Self {
        Self {
            name: name.to_owned(),
            email: email.to_owned(),
        }
    }
}

impl CommitOptions {
    pub fn new(message: &str, author: GitIdentity, date: &str) -> Self {
        Self {
            message: message.to_owned(),
            committer: author.clone(),
            author,
            author_date: date.to_owned(),
            committer_date: date.to_owned(),
        }
    }

    pub fn committer(mut self, committer: GitIdentity) -> Self {
        self.committer = committer;
        self
    }

    pub fn committer_date(mut self, date: &str) -> Self {
        self.committer_date = date.to_owned();
        self
    }
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}
