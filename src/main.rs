use std::cell::RefCell;
use std::cmp::Reverse;
use std::env::current_dir;
use std::io::BufWriter;

use console::{style, Term};
use itertools::Itertools;
use rayon::prelude::*;
use regex::Regex;
use structopt::StructOpt;
use thread_local::ThreadLocal;

use fclones::config::*;
use fclones::files::*;
use fclones::group::*;
use fclones::log::Log;
use fclones::path::Path;
use fclones::pattern::ESCAPE_CHAR;
use fclones::report::Reporter;
use fclones::walk::Walk;
use indoc::indoc;

const MIN_PREFIX_LEN: FileLen = FileLen(4096);
const MAX_PREFIX_LEN: FileLen = FileLen(4 * MIN_PREFIX_LEN.0);
const SUFFIX_LEN: FileLen = MIN_PREFIX_LEN;

struct AppCtx {
    config: Config,
    log: Log,
}

/// Configures global thread pool to use desired number of threads
fn configure_thread_pool(parallelism: usize) {
    rayon::ThreadPoolBuilder::new()
        .num_threads(parallelism)
        .build_global()
        .unwrap();
}

/// Unless `duplicate_links` is set to true,
/// remove duplicated `FileInfo` entries with the same inode and device id from the list.
fn prune_links_if_needed(ctx: &AppCtx, files: Vec<FileInfoNoLen>) -> Vec<FileInfoNoLen> {
    // if we allow non-unique, or they are already unique, don't deduplicate
    if ctx.config.hard_links || files.iter().unique_by(|i| i.id_hash).count() == files.len() {
        files
    } else {
        deduplicate(files, |p| file_id_or_log_err(p, &ctx.log))
    }
}

/// Deduplicates paths (based on the path text, not file contents)
fn remove_duplicate_paths(files: Vec<FileInfoNoLen>) -> Vec<FileInfoNoLen> {
    deduplicate(files, |p| p.hash128())
}

/// Deduplicates a vector of file information objects by given key.
fn deduplicate<K>(files: Vec<FileInfoNoLen>, key: impl Fn(&Path) -> K) -> Vec<FileInfoNoLen>
where
    K: Eq + std::hash::Hash,
{
    files
        .into_iter()
        // need to unpack the struct before accessing path reference
        // referencing fields of a packed struct directly is UB, because they may be unaligned
        .map(|i| (i.path, i.id_hash))
        .unique_by(|(path, _)| (key)(path))
        .map(|(path, id_hash)| FileInfoNoLen { path, id_hash })
        .collect()
}

/// Walks the directory tree and collects matching files in parallel into a vector
fn scan_files(ctx: &mut AppCtx) -> Vec<Vec<FileInfo>> {
    let base_dir = Path::from(current_dir().unwrap_or_default());
    let path_selector = ctx.config.path_selector(&ctx.log, &base_dir);
    let file_collector = ThreadLocal::new();
    let spinner = ctx.log.spinner("[1/7] Scanning files");
    let spinner_tick = &|_: &Path| spinner.tick();

    let config = &ctx.config;
    let min_size = config.min_size;
    let max_size = config.max_size.unwrap_or(FileLen::MAX);

    let mut walk = Walk::new();
    walk.recursive = config.recursive;
    walk.depth = config.depth.unwrap_or(usize::MAX);
    walk.skip_hidden = config.skip_hidden;
    walk.follow_links = config.follow_links;
    walk.path_selector = path_selector;
    walk.log = Some(&ctx.log);
    walk.on_visit = spinner_tick;
    walk.run(ctx.config.input_paths(), |path| {
        file_info_or_log_err(path, &ctx.log)
            .into_iter()
            .filter(|info| {
                let l = info.len;
                l >= min_size && l <= max_size
            })
            .for_each(|info| {
                let vec = file_collector.get_or(|| RefCell::new(Vec::new()));
                vec.borrow_mut().push(info);
            });
    });

    ctx.log
        .info(format!("Scanned {} file entries", spinner.position()));

    let files: Vec<_> = file_collector.into_iter().map(|r| r.into_inner()).collect();

    let file_count: usize = files.iter().map(|v| v.len()).sum();
    let total_size: u64 = files.iter().flat_map(|v| v.iter().map(|i| i.len.0)).sum();
    ctx.log.info(format!(
        "Found {} ({}) files matching selection criteria",
        file_count,
        FileLen(total_size)
    ));
    files
}

fn group_by_size(ctx: &mut AppCtx, files: Vec<Vec<FileInfo>>) -> Vec<FileGroup<FileInfoNoLen>> {
    let file_count: usize = files.iter().map(|v| v.len()).sum();
    let progress = ctx
        .log
        .progress_bar("[2/7] Grouping by size", file_count as u64);

    let mut groups = GroupMap::new(|info: FileInfo| (info.len, info.drop_len()));
    for files in files.into_iter() {
        for file in files.into_iter() {
            progress.tick();
            groups.add(file);
        }
    }
    let rf_over = ctx.config.rf_over();
    let rf_under = ctx.config.rf_under();

    let groups: Vec<_> = groups
        .into_iter()
        .map(|(l, files)| (l, remove_duplicate_paths(files)))
        .filter(|(_, files)| files.len() >= rf_over)
        .map(|(l, files)| FileGroup {
            len: l,
            hash: None,
            files,
        })
        .collect();

    let count: usize = groups.selected_count(rf_over, rf_under);
    let bytes: FileLen = groups.selected_size(rf_over, rf_under);
    ctx.log.info(format!(
        "Found {} ({}) candidates after grouping by size",
        count, bytes
    ));
    groups
}

fn to_group_of_paths(f: FileGroup<FileInfoNoLen>) -> FileGroup<Path> {
    FileGroup {
        len: f.len,
        hash: None,
        files: f.files.into_iter().map(|i| i.path).collect(),
    }
}

/// Drops file id hashes from file info and returns groups of paths
fn drop_hard_link_info(groups: Vec<FileGroup<FileInfoNoLen>>) -> Vec<FileGroup<Path>> {
    groups.into_par_iter().map(to_group_of_paths).collect()
}

fn prune_hard_links(
    ctx: &mut AppCtx,
    groups: Vec<FileGroup<FileInfoNoLen>>,
) -> Vec<FileGroup<Path>> {
    let file_count: usize = groups.total_count();
    let progress = ctx
        .log
        .progress_bar("[3/7] Pruning hard links", file_count as u64);

    let rf_over = ctx.config.rf_over();
    let rf_under = ctx.config.rf_under();

    let groups: Vec<_> = groups
        .into_par_iter()
        .inspect(|g| progress.inc(g.files.len()))
        .map(|mut g| {
            g.files = prune_links_if_needed(&ctx, g.files);
            g
        })
        .filter(|g| g.files.len() >= rf_over)
        .map(to_group_of_paths)
        .collect();

    let count: usize = groups.selected_count(rf_over, rf_under);
    let bytes: FileLen = groups.selected_size(rf_over, rf_under);
    ctx.log.info(format!(
        "Found {} ({}) candidates after pruning hard-links",
        count, bytes
    ));
    groups
}

fn group_by_prefix(ctx: &mut AppCtx, groups: Vec<FileGroup<Path>>) -> Vec<FileGroup<Path>> {
    let remaining_files = groups.iter().filter(|g| g.files.len() > 1).total_count();
    let progress = ctx
        .log
        .progress_bar("[4/7] Grouping by prefix", remaining_files as u64);

    let rf_over = ctx.config.rf_over();
    let rf_under = ctx.config.rf_under();

    let groups: Vec<_> = groups.split(rf_over + 1, |len, _hash, path| {
        progress.tick();
        let prefix_len = if len <= MAX_PREFIX_LEN {
            len
        } else {
            MIN_PREFIX_LEN
        };
        let caching = if len <= MAX_PREFIX_LEN {
            Caching::Default
        } else {
            Caching::Random
        };
        file_hash_or_log_err(path, FilePos(0), prefix_len, caching, |_| {}, &ctx.log)
    });

    let count = groups.selected_count(rf_over, rf_under);
    let bytes = groups.selected_size(rf_over, rf_under);
    ctx.log.info(format!(
        "Found {} ({}) candidates after grouping by prefix",
        count, bytes
    ));
    groups
}

fn group_by_suffix(ctx: &mut AppCtx, groups: Vec<FileGroup<Path>>) -> Vec<FileGroup<Path>> {
    let needs_processing = |len: FileLen| len >= MAX_PREFIX_LEN + SUFFIX_LEN * 2;
    let remaining_files = groups
        .iter()
        .filter(|&g| (needs_processing)(g.len) && g.files.len() > 1)
        .total_count();

    let progress = ctx
        .log
        .progress_bar("[5/7] Grouping by suffix", remaining_files as u64);

    let rf_over = ctx.config.rf_over();
    let rf_under = ctx.config.rf_under();

    let groups: Vec<_> = groups.split(rf_over + 1, |len, hash, path| {
        if (needs_processing)(len) {
            progress.tick();
            file_hash_or_log_err(
                path,
                (len - SUFFIX_LEN).as_pos(),
                SUFFIX_LEN,
                Caching::Default,
                |_| {},
                &ctx.log,
            )
        } else {
            hash
        }
    });

    let count = groups.selected_count(rf_over, rf_under);
    let bytes = groups.selected_size(rf_over, rf_under);
    ctx.log.info(format!(
        "Found {} ({}) candidates after grouping by suffix",
        count, bytes
    ));
    groups
}

fn group_by_contents(ctx: &mut AppCtx, groups: Vec<FileGroup<Path>>) -> Vec<FileGroup<Path>> {
    let needs_processing = |len: FileLen| len >= MAX_PREFIX_LEN;
    let bytes_to_scan = groups
        .iter()
        .filter(|&g| (needs_processing)(g.len) && g.files.len() > 1)
        .total_size();
    let progress = ctx
        .log
        .bytes_progress_bar("[6/7] Grouping by contents", bytes_to_scan.0);
    let rf_over = ctx.config.rf_over();
    let rf_under = ctx.config.rf_under();

    let groups: Vec<_> = groups.split(rf_over + 1, |len, hash, path| {
        if (needs_processing)(len) {
            file_hash_or_log_err(
                path,
                FilePos(0),
                len,
                Caching::Sequential,
                |delta| progress.inc(delta),
                &ctx.log,
            )
        } else {
            hash
        }
    });

    let count = groups.selected_count(rf_over, rf_under);
    let bytes = groups.selected_size(rf_over, rf_under);
    ctx.log.info(format!(
        "Found {} ({}) {} files",
        count,
        bytes,
        ctx.config.search_type()
    ));
    groups
}

fn write_report(ctx: &mut AppCtx, groups: &mut Vec<FileGroup<Path>>) {
    let stdout = Term::stdout();
    let remaining_files = groups.total_count();
    let progress = ctx
        .log
        .progress_bar("[7/7] Writing report", remaining_files as u64);

    // No progress bar when we write to terminal
    if stdout.is_term() {
        progress.finish_and_clear()
    }
    let out = BufWriter::new(stdout);
    let mut reporter = Reporter::new(out, progress);
    let result = match &ctx.config.format {
        OutputFormat::Text => reporter.write_as_text(groups),
        OutputFormat::Csv => reporter.write_as_csv(groups),
        OutputFormat::Json => reporter.write_as_json(groups),
    };
    match result {
        Ok(()) => (),
        Err(e) => ctx.log.err(format!("Failed to write report: {}", e)),
    }
}

fn paint_help(s: &str) -> String {
    let escape_regex = "{escape}";
    let code_regex = Regex::new(r"`([^`]+)`").unwrap();
    let title_regex = Regex::new(r"(?m)^# *(.*?)$").unwrap();
    let s = s.replace(escape_regex, ESCAPE_CHAR);
    let s = code_regex.replace_all(s.as_ref(), style("$1").green().to_string().as_str());
    title_regex
        .replace_all(s.as_ref(), style("$1").yellow().to_string().as_str())
        .to_string()
}

fn main() {
    let after_help = &paint_help(indoc!(
        "
    # PATTERN SYNTAX:
        Options `-n` `-p` and `-e` accept extended glob patterns.
        The following wildcards can be used:
        `?`      matches any character except the directory separator
        `[a-z]`  matches one of the characters or character ranges given in the square brackets
        `[!a-z]` matches any character that is not given in the square brackets
        `*`      matches any sequence of characters except the directory separator
        `**`     matches any sequence of characters
        `{a,b}`  matches exactly one pattern from the comma-separated patterns given
               inside the curly brackets
        `@(a|b)` same as `{a,b}`
        `?(a|b)` matches at most one occurrence of the pattern inside the brackets
        `+(a|b)` matches at least occurrence of the patterns given inside the brackets
        `*(a|b)` matches any number of occurrences of the patterns given inside the brackets
        `!(a|b)` matches anything that doesn't match any of the patterns given inside the brackets
        `{escape}`      escapes wildcards, e.g. `{escape}?` would match `?` literally
    "
    ));

    let clap = Config::clap().after_help(after_help.as_str());
    let config: Config = Config::from_clap(&clap.get_matches());

    configure_thread_pool(config.threads);

    let log = Log::new();
    let mut ctx = AppCtx { log, config };

    let mut contents_groups = process(&mut ctx);
    write_report(&mut ctx, &mut contents_groups)
}

// Extracted for testing
fn process(mut ctx: &mut AppCtx) -> Vec<FileGroup<Path>> {
    let matching_files = scan_files(&mut ctx);
    let size_groups = group_by_size(&mut ctx, matching_files);
    let size_groups_pruned = if ctx.config.hard_links {
        drop_hard_link_info(size_groups)
    } else {
        prune_hard_links(&mut ctx, size_groups)
    };
    let prefix_groups = group_by_prefix(&mut ctx, size_groups_pruned);
    let suffix_groups = group_by_suffix(&mut ctx, prefix_groups);
    let mut contents_groups = group_by_contents(&mut ctx, suffix_groups);
    contents_groups.retain(|g| g.files.len() < ctx.config.rf_under());
    contents_groups.par_sort_by_key(|g| Reverse(g.len));
    contents_groups.par_iter_mut().for_each(|g| g.files.sort());
    contents_groups
}

#[cfg(test)]
mod test {
    use std::io::Write;
    use std::path::PathBuf;

    use fclones::path::Path;
    use fclones::util::test::*;

    use super::*;
    use std::fs::{hard_link, OpenOptions};

    #[test]
    fn identical_small_files() {
        with_dir("target/test/main/identical_small_files", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            write_test_file(&file1, b"aaa", b"", b"");
            write_test_file(&file2, b"aaa", b"", b"");

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1, file2];
            let results = process(&mut ctx);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].len, FileLen(3));
            assert_eq!(results[0].files.len(), 2);
        });
    }

    #[test]
    fn identical_large_files() {
        with_dir("target/test/main/identical_large_files", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            write_test_file(
                &file1,
                &[0; MAX_PREFIX_LEN.0 as usize],
                &[1; 4096],
                &[2; 4096],
            );
            write_test_file(
                &file2,
                &[0; MAX_PREFIX_LEN.0 as usize],
                &[1; 4096],
                &[2; 4096],
            );

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1, file2];

            let results = process(&mut ctx);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].files.len(), 2);
        });
    }

    #[test]
    fn files_differing_by_size() {
        with_dir("target/test/main/files_differing_by_size", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            write_test_file(&file1, b"aaaa", b"", b"");
            write_test_file(&file2, b"aaa", b"", b"");

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file2.clone()];
            ctx.config.rf_over = Some(0);

            let results = process(&mut ctx);
            assert_eq!(results.len(), 2);
            assert_eq!(
                results[0].files,
                vec![Path::from(file1.canonicalize().unwrap())]
            );
            assert_eq!(
                results[1].files,
                vec![Path::from(file2.canonicalize().unwrap())]
            );
        });
    }

    #[test]
    fn files_differing_by_prefix() {
        with_dir("target/test/main/files_differing_by_prefix", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            write_test_file(&file1, b"aaa", b"", b"");
            write_test_file(&file2, b"bbb", b"", b"");

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file2.clone()];
            ctx.config.unique = true;

            let results = process(&mut ctx);
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].files.len(), 1);
            assert_eq!(results[1].files.len(), 1);
        });
    }

    #[test]
    fn files_differing_by_suffix() {
        with_dir("target/test/main/files_differing_by_suffix", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            let prefix = [0; MAX_PREFIX_LEN.0 as usize];
            let mid = [1; (MAX_PREFIX_LEN.0 + 2 * SUFFIX_LEN.0) as usize];
            write_test_file(&file1, &prefix, &mid, b"suffix1");
            write_test_file(&file2, &prefix, &mid, b"suffix2");

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file2.clone()];
            ctx.config.unique = true;

            let results = process(&mut ctx);
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].files.len(), 1);
            assert_eq!(results[1].files.len(), 1);
        });
    }

    #[test]
    fn files_differing_by_middle() {
        with_dir("target/test/main/files_differing_by_middle", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            let prefix = [0; MAX_PREFIX_LEN.0 as usize];
            let suffix = [1; (SUFFIX_LEN.0 * 2) as usize];
            write_test_file(&file1, &prefix, b"middle1", &suffix);
            write_test_file(&file2, &prefix, b"middle2", &suffix);

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file2.clone()];
            ctx.config.unique = true;

            let results = process(&mut ctx);
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].files.len(), 1);
            assert_eq!(results[1].files.len(), 1);
        });
    }

    #[test]
    fn hard_links() {
        with_dir("target/test/main/hard_links", |root| {
            let file1 = root.join("file1");
            let file2 = root.join("file2");
            write_test_file(&file1, b"aaa", b"", b"");
            hard_link(&file1, &file2).unwrap();

            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file2.clone()];
            ctx.config.unique = true;

            let results = process(&mut ctx);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].files.len(), 1);
        });
    }

    #[test]
    fn duplicate_input_files() {
        with_dir("target/test/main/duplicate_input_files", |root| {
            let file1 = root.join("file1");
            write_test_file(&file1, b"foo", b"", b"");
            let mut ctx = test_ctx();
            ctx.config.paths = vec![file1.clone(), file1.clone(), file1.clone()];
            ctx.config.unique = true;
            ctx.config.hard_links = true;

            let results = process(&mut ctx);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].files.len(), 1);
        });
    }

    #[test]
    #[cfg(unix)]
    fn duplicate_input_files_non_canonical() {
        use std::os::unix::fs::symlink;

        with_dir(
            "target/test/main/duplicate_input_files_non_canonical",
            |root| {
                let dir = root.join("dir");
                symlink(&root, &dir).unwrap();

                let file1 = root.join("file1");
                let file2 = root.join("dir/file1");
                write_test_file(&file1, b"foo", b"", b"");

                let mut ctx = test_ctx();
                ctx.config.paths = vec![file1.clone(), file2.clone()];
                ctx.config.unique = true;
                ctx.config.hard_links = true;

                let results = process(&mut ctx);
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].files.len(), 1);
            },
        );
    }

    fn write_test_file(path: &PathBuf, prefix: &[u8], mid: &[u8], suffix: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        file.write(prefix).unwrap();
        file.write(mid).unwrap();
        file.write(suffix).unwrap();
    }

    fn test_ctx() -> AppCtx {
        let mut log = Log::new();
        log.no_progress = true;
        let config: Config = Default::default();
        AppCtx { log, config }
    }
}
