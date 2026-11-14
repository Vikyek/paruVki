use std::path::Path;

use crate::config::Config;
use crate::config::SortBy;
use crate::fmt::{color_repo, escape_control, link_str, print_indent};
use crate::util::{NumberMenu, input, is_arch_repo, now_secs};
use crate::{info, printtr};

use ansiterm::Style;
use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use indicatif::HumanBytes;
use raur::{Raur, SearchBy};
use regex::RegexSet;
use reqwest::get;
use srcinfo::Srcinfo;
use tr::tr;

#[derive(Debug)]
pub enum AnyPkg<'a> {
    RepoPkg(&'a alpm::Package),
    AurPkg(&'a raur::Package),
    Custom(&'a str, &'a Srcinfo, &'a srcinfo::Package),
}

pub async fn search(config: &Config) -> Result<i32> {
    let quiet = config.args.has_arg("q", "quiet");

    let mut repo_pkgs = search_repos(config, &config.targets)?;

    let targets = config
        .targets
        .iter()
        .map(|t| t.to_lowercase())
        .collect::<Vec<_>>();

    let custom_pkgs = search_pkgbuilds(config, &targets)?;

    let mut pkgs = search_aur(config, &targets)
        .await
        .context(tr!("aur search failed"))?;

    deduplicate_search_results(&mut repo_pkgs, &mut pkgs);

    let print_custom = || {
        for (repo, srcinfo, pkg) in &custom_pkgs {
            let path = &config
                .pkgbuild_repos
                .repo(repo)
                .unwrap()
                .base(config, &srcinfo.base.pkgbase)
                .unwrap()
                .path;
            print_pkgbuild_pkg(config, repo, path, srcinfo, pkg, quiet);
        }
    };

    if config.sort_mode.is_top_down(false) {
        for pkg in &repo_pkgs {
            print_alpm_pkg(config, pkg, quiet);
        }
        print_custom();
        for pkg in &pkgs {
            print_pkg(config, pkg, quiet)
        }
    } else {
        for pkg in pkgs.iter().rev() {
            print_pkg(config, pkg, quiet)
        }
        print_custom();
        for pkg in repo_pkgs.iter().rev() {
            print_alpm_pkg(config, pkg, quiet);
        }
    }

    Ok((repo_pkgs.is_empty() && pkgs.is_empty()) as i32)
}

fn search_pkgbuilds<'a>(
    config: &'a Config,
    targets: &[String],
) -> Result<Vec<(&'a str, &'a Srcinfo, &'a srcinfo::Package)>> {
    if !config.mode.pkgbuild() {
        return Ok(Vec::new());
    }

    let regex = RegexSet::new(targets)?;
    let mut ret = Vec::new();

    for repo in &config.pkgbuild_repos.repos {
        for base in repo.pkgs(config) {
            let base = &base.srcinfo;
            for pkg in &base.pkgs {
                if targets.is_empty()
                    || regex.is_match(&base.base.pkgbase)
                    || regex.is_match(&pkg.pkgname)
                    || pkg.pkgdesc.iter().any(|d| regex.is_match(d))
                    || pkg
                        .provides
                        .iter()
                        .flat_map(|p| p.values())
                        .any(|p| regex.is_match(p))
                    || pkg.groups.iter().any(|g| regex.is_match(g))
                {
                    ret.push((repo.name.as_str(), base, pkg))
                }
            }
        }
    }

    Ok(ret)
}

fn search_local<'a>(config: &'a Config, targets: &[String]) -> Result<Vec<&'a alpm::Package>> {
    let mut ret = Vec::new();

    if targets.is_empty() {
        ret.extend(config.alpm.localdb().pkgs());
    } else {
        let pkgs = config.alpm.localdb().search(targets.iter())?;
        ret.extend(pkgs);
    };

    if config.limit != 0 {
        ret.truncate(config.limit);
    }

    Ok(ret)
}

fn search_repos<'a>(config: &'a Config, targets: &[String]) -> Result<Vec<&'a alpm::Package>> {
    if targets.is_empty() || !config.mode.repo() {
        return Ok(Vec::new());
    }

    let mut ret = Vec::new();

    for db in config.alpm.syncdbs() {
        let pkgs = db.search(targets.iter())?;
        ret.extend(pkgs);
    }

    if config.limit != 0 {
        ret.truncate(config.limit);
    }

    Ok(ret)
}

async fn search_target(config: &Config, targets: &mut Vec<String>) -> Result<Vec<raur::Package>> {
    let by = config.search_by;
    let mut pkgs = Ok(Vec::new());
    let mut index = 0;

    for (i, target) in targets.iter().enumerate() {
        index = i;
        pkgs = config.raur.as_ref().unwrap().search_by(target, by).await;
        if !matches!(pkgs, Err(raur::Error::Aur(_))) {
            break;
        }
    }

    if pkgs.is_ok() {
        targets.remove(index);
    }

    Ok(pkgs?)
}

async fn search_aur_regex(config: &Config, targets: &[String]) -> Result<Vec<raur::Package>> {
    let url = config.aur_url.join("packages.gz")?;
    let resp = get(url.clone())
        .await
        .with_context(|| format!("get {}", url))?;
    let success = resp.status().is_success();
    ensure!(success, "get {}: {}", url, resp.status());

    let data = resp.bytes().await?;
    let decoder = GzDecoder::new(&*data);
    let data =
        std::io::read_to_string(decoder).with_context(|| tr!("failed to decode package list"))?;

    let regex = RegexSet::new(targets)?;

    let pkgs = data
        .lines()
        .filter(|pkg| regex.is_match(pkg))
        .collect::<Vec<_>>();
    ensure!(pkgs.len() < 2000, "too many packages");
    let pkgs = config.raur.as_ref().unwrap().info(&pkgs).await?;
    Ok(pkgs)
}

async fn search_aur(config: &Config, targets: &[String]) -> Result<Vec<raur::Package>> {
    if targets.is_empty() || !config.mode.aur() {
        return Ok(Vec::new());
    }

    let mut matches = if config.args.has_arg("x", "regex") {
        search_aur_regex(config, targets).await?
    } else {
        let mut targets = targets.iter().map(|t| t.to_lowercase()).collect::<Vec<_>>();
        targets.sort_by_key(|t| t.len());

        let mut matches = Vec::new();

        let by = config.search_by;

        if by == SearchBy::NameDesc {
            let pkgs = search_target(config, &mut targets).await?;
            matches.extend(pkgs);
            matches.retain(|p| {
                let name = p.name.to_lowercase();
                let description = p
                    .description
                    .as_ref()
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                targets
                    .iter()
                    .all(|t| name.contains(t) | description.contains(t))
            });
        } else if by == SearchBy::Name {
            let pkgs = search_target(config, &mut targets).await?;
            matches.extend(pkgs);
            matches.retain(|p| targets.iter().all(|t| p.name.to_lowercase().contains(t)));
        } else {
            for target in targets {
                let pkgs = config.raur.as_ref().unwrap().search_by(target, by).await?;
                matches.extend(pkgs);
            }
        }

        matches
    };

    match config.sort_by {
        SortBy::Votes => matches.sort_by_key(|a| std::cmp::Reverse(a.num_votes)),
        SortBy::Popularity => {
            matches.sort_by(|a, b| b.popularity.partial_cmp(&a.popularity).unwrap())
        }
        SortBy::Id => matches.sort_by_key(|p| p.id),
        SortBy::Name => matches.sort_by(|a, b| a.name.cmp(&b.name)),
        SortBy::Base => matches.sort_by(|a, b| a.package_base.cmp(&b.package_base)),
        SortBy::Submitted => matches.sort_by_key(|p| p.first_submitted),
        SortBy::Modified => matches.sort_by_key(|p| p.last_modified),
        _ => (),
    }

    if let Some(max) = config.min_release_age {
        let now = now_secs();
        let max = max.as_secs() as i64;
        matches.retain(|p| !config.is_too_new(&p.name, p.last_modified, now, max));
    }

    if config.limit != 0 {
        matches.truncate(config.limit);
    }

    Ok(matches)
}

fn print_pkgbuild_pkg(
    config: &Config,
    repo: &str,
    path: &Path,
    srcinfo: &Srcinfo,
    pkg: &srcinfo::Package,
    quiet: bool,
) {
    if quiet {
        println!("{}", escape_control(&pkg.pkgname));
        return;
    }

    let c = config.color;

    let name = if let Some(url) = &pkg.url {
        link_str(
            c.enabled,
            &c.ss_name.paint(escape_control(&pkg.pkgname)),
            url,
        )
    } else {
        c.ss_name.paint(escape_control(&pkg.pkgname)).to_string()
    };

    print!(
        "{}/{} {}",
        color_repo(c.enabled, &escape_control(repo)),
        name,
        c.ss_ver.paint(escape_control(&srcinfo.version())),
    );

    if let Ok(repo_pkg) = config.alpm.localdb().pkg(&*pkg.pkgname) {
        let installed = if repo_pkg.version().as_str() != srcinfo.version() {
            tr!(
                "[installed: {}]",
                escape_control(repo_pkg.version().as_str())
            )
        } else {
            tr!("[installed]")
        };

        print!(" {}", c.ss_installed.paint(installed));
    }

    let none = tr!("None");
    print!("\n    ");
    let desc = pkg
        .pkgdesc
        .as_deref()
        .unwrap_or(&none)
        .split_whitespace()
        .map(escape_control);
    print_indent(Style::new(), 4, 4, config.cols, " ", desc);

    if config.args.count("s", "search") > 1 {
        info::print(c, 14, config.cols, "    Path", &path.display().to_string());
    }
}

fn print_pkg(config: &Config, pkg: &raur::Package, quiet: bool) {
    if quiet {
        println!("{}", escape_control(&pkg.name));
        return;
    }

    let c = config.color;
    let stats = format!("+{} ~{:.2}", pkg.num_votes, pkg.popularity);

    let aur = color_repo(c.enabled, "aur");
    let aur = if let Ok(url) = config.aur_url.join(&format!("packages/{}", pkg.name)) {
        link_str(c.enabled, &aur, url.as_str())
    } else {
        aur
    };
    let name = if let Some(url) = &pkg.url {
        link_str(c.enabled, &c.ss_name.paint(escape_control(&pkg.name)), url)
    } else {
        c.ss_name.paint(escape_control(&pkg.name)).to_string()
    };
    print!(
        "{}/{} {} [{}]",
        color_repo(c.enabled, &aur),
        c.ss_name.paint(name),
        c.ss_ver.paint(escape_control(&pkg.version)),
        c.ss_stats.paint(stats),
    );

    if let Some(date) = pkg.out_of_date {
        let date = tr!("[out-of-date: {}]", crate::fmt::ymd(date));
        print!(" {}", c.ss_ood.paint(date));
    }

    if let Ok(repo_pkg) = config.alpm.localdb().pkg(&*pkg.name) {
        let installed = if repo_pkg.version().as_str() != pkg.version {
            tr!(
                "[installed: {}]",
                escape_control(repo_pkg.version().as_str())
            )
        } else {
            tr!("[installed]")
        };

        print!(" {}", c.ss_installed.paint(installed));
    }

    if pkg.maintainer.is_none() {
        print!(" {}", c.ss_orphaned.paint(tr!("[orphaned]")));
    }

    let none = tr!("None");
    print!("\n    ");
    let desc = pkg
        .description
        .as_deref()
        .unwrap_or(&none)
        .split_whitespace()
        .map(escape_control);
    print_indent(Style::new(), 4, 4, config.cols, " ", desc);

    if config.args.count("s", "search") > 1 {
        if let Some(ref url) = pkg.url {
            info::print(c, 14, config.cols, "    URL", url);
        }

        let aur_url = format!("{}packages/{}", config.aur_url, pkg.package_base);
        info::print(c, 14, config.cols, "    AUR URL", aur_url.as_str());
    }
}

fn print_alpm_pkg(config: &Config, pkg: &alpm::Package, quiet: bool) {
    if quiet {
        println!("{}", escape_control(pkg.name()));
        return;
    }

    let c = config.color;
    let stats = format!(
        "{} {}",
        HumanBytes(pkg.download_size() as u64),
        HumanBytes(pkg.isize() as u64)
    );
    let ver: &str = pkg.version().as_ref();
    let mut repo = color_repo(c.enabled, &escape_control(pkg.db().unwrap().name()));
    if is_arch_repo(pkg.db().unwrap().name())
        && let Ok(url) = config.arch_url.join(&format!(
            "packages/{}/{}/{}/",
            pkg.db().unwrap().name(),
            pkg.arch().unwrap_or("any"),
            pkg.name()
        ))
    {
        repo = link_str(c.enabled, &repo, url.as_str());
    }

    let name = if let Some(url) = pkg.url() {
        link_str(c.enabled, &c.ss_name.paint(escape_control(pkg.name())), url)
    } else {
        c.ss_name.paint(escape_control(pkg.name())).to_string()
    };

    print!(
        "{}/{} {} [{}]",
        color_repo(c.enabled, &repo),
        c.ss_name.paint(name),
        c.ss_ver.paint(escape_control(ver)),
        c.ss_stats.paint(stats),
    );

    if let Ok(repo_pkg) = config.alpm.localdb().pkg(pkg.name()) {
        let installed = if repo_pkg.version() != pkg.version() {
            tr!(
                "[installed: {}]",
                escape_control(repo_pkg.version().as_str())
            )
        } else {
            tr!("[installed]")
        };

        print!(" {}", c.ss_installed.paint(installed));
    }

    if !pkg.groups().is_empty() {
        print!(" {}", c.ss_orphaned.paint("("));
        print!(
            "{}",
            c.ss_orphaned
                .paint(escape_control(pkg.groups().first().unwrap()))
        );
        for group in pkg.groups().iter().skip(1) {
            print!(" {}", c.ss_orphaned.paint(escape_control(group)));
        }
        print!("{}", c.ss_orphaned.paint(")"));
    }

    print!("\n    ");
    let desc = pkg.desc();
    let desc = desc
        .unwrap_or_default()
        .split_whitespace()
        .map(escape_control);
    print_indent(Style::new(), 4, 4, config.cols, " ", desc);

    if config.args.count("s", "search") > 1
        && let Some(url) = pkg.url()
    {
        info::print(c, 14, config.cols, "    URL", url);
    }
}

pub fn interactive_search_local(config: &mut Config) -> Result<()> {
    let mut all_pkgs = Vec::new();
    let repo_pkgs = search_local(config, &config.targets)?;

    for pkg in repo_pkgs {
        all_pkgs.push(AnyPkg::RepoPkg(pkg));
    }

    let was_results = all_pkgs.is_empty();
    let targs = interactive_menu(config, all_pkgs, false)?;
    if targs.is_empty() && !was_results {
        printtr!(" there is nothing to do");
    }
    config.targets = targs.clone();
    config.args.targets = targs;
    Ok(())
}

pub async fn interactive_search(config: &mut Config, install: bool) -> Result<()> {
    let mut repo_pkgs = search_repos(config, &config.targets)?;
    let custom_pkgs = search_pkgbuilds(config, &config.targets)?;
    let mut aur_pkgs = search_aur(config, &config.targets).await?;

    deduplicate_search_results(&mut repo_pkgs, &mut aur_pkgs);

    let mut all_pkgs = Vec::new();

    for pkg in repo_pkgs {
        all_pkgs.push(AnyPkg::RepoPkg(pkg));
    }
    for (repo, base, pkg) in custom_pkgs {
        all_pkgs.push(AnyPkg::Custom(repo, base, pkg));
    }
    for pkg in &aur_pkgs {
        all_pkgs.push(AnyPkg::AurPkg(pkg));
    }

    let was_results = all_pkgs.is_empty();
    let targs = interactive_menu(config, all_pkgs, install)?;
    if targs.is_empty() && !was_results {
        printtr!(" there is nothing to do");
    }
    config.targets = targs.clone();
    config.args.targets = targs;
    Ok(())
}

pub fn interactive_menu(
    config: &Config,
    mut all_pkgs: Vec<AnyPkg<'_>>,
    install: bool,
) -> Result<Vec<String>> {
    let pad = all_pkgs.len().to_string().len();

    if all_pkgs.is_empty() {
        printtr!("no packages match search");
        return Ok(Vec::new());
    }

    let indexes = all_pkgs
        .iter()
        .enumerate()
        .filter_map(|(n, pkg)| {
            let name = match pkg {
                AnyPkg::RepoPkg(pkg) => pkg.name(),
                AnyPkg::AurPkg(pkg) => pkg.name.as_str(),
                AnyPkg::Custom(_, _, pkg) => pkg.pkgname.as_str(),
            };

            if config.targets.iter().any(|targ| targ == name) {
                Some(n)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for (i, n) in indexes.iter().rev().enumerate() {
        let pkg = all_pkgs.remove(i + n);
        all_pkgs.insert(0, pkg);
    }

    if config.sort_mode.is_top_down(true) {
        for (n, pkg) in all_pkgs.iter().enumerate() {
            print_any_pkg(config, n, pad, pkg)
        }
    } else {
        for (n, pkg) in all_pkgs.iter().enumerate().rev() {
            print_any_pkg(config, n, pad, pkg)
        }
    }

    let input = if install {
        input(config, &tr!("Packages to install (eg: 1 2 3, 1-3):"))
    } else {
        input(config, &tr!("Select packages (eg: 1 2 3, 1-3):"))
    };

    if input.trim().is_empty() {
        return Ok(Vec::new());
    }

    let menu = NumberMenu::new(&input);
    let mut pkgs = Vec::new();

    if config.sort_mode.is_top_down(true) {
        for (n, pkg) in all_pkgs.iter().enumerate() {
            if menu.contains(n + 1, "") {
                match pkg {
                    AnyPkg::RepoPkg(pkg) => {
                        pkgs.push(format!("{}/{}", pkg.db().unwrap().name(), pkg.name()))
                    }
                    AnyPkg::AurPkg(pkg) => {
                        pkgs.push(format!("{}/{}", config.aur_namespace(), pkg.name))
                    }
                    AnyPkg::Custom(repo, _, pkg) => pkgs.push(format!("{}/{}", repo, pkg.pkgname)),
                }
            }
        }
    } else {
        for (n, pkg) in all_pkgs.iter().enumerate().rev() {
            if menu.contains(n + 1, "") {
                match pkg {
                    AnyPkg::RepoPkg(pkg) => {
                        pkgs.push(format!("{}/{}", pkg.db().unwrap().name(), pkg.name()))
                    }
                    AnyPkg::AurPkg(pkg) => {
                        pkgs.push(format!("{}/{}", config.aur_namespace(), pkg.name))
                    }
                    AnyPkg::Custom(repo, _, pkg) => pkgs.push(format!("{}/{}", repo, pkg.pkgname)),
                }
            }
        }
    }

    Ok(pkgs)
}

fn print_any_pkg(config: &Config, n: usize, pad: usize, pkg: &AnyPkg) {
    let c = config.color;
    match pkg {
        AnyPkg::RepoPkg(pkg) => {
            let n = format!("{:>pad$}", n + 1, pad = pad);
            print!("{} ", c.number_menu.paint(n));
            print_alpm_pkg(config, pkg, false)
        }
        AnyPkg::AurPkg(pkg) => {
            let n = format!("{:>pad$}", n + 1, pad = pad);
            print!("{} ", c.number_menu.paint(n));
            print_pkg(config, pkg, false)
        }
        AnyPkg::Custom(repo, base, pkg) => {
            let n = format!("{:>pad$}", n + 1, pad = pad);
            print!("{} ", c.number_menu.paint(n));
            let path = &config
                .pkgbuild_repos
                .repo(repo)
                .unwrap()
                .base(config, &base.base.pkgbase)
                .unwrap()
                .path;
            print_pkgbuild_pkg(config, repo, path, base, pkg, false)
        }
    };
}

pub fn deduplicate_search_results<'a>(
    repo_pkgs: &mut Vec<&'a alpm::Package>,
    aur_pkgs: &mut Vec<raur::Package>,
) {
    // 1. Deduplicate non-official sync repo vs official Arch repo packages
    let mut remove_repo_indices = std::collections::HashSet::new();
    for i in 0..repo_pkgs.len() {
        for j in 0..repo_pkgs.len() {
            if i == j {
                continue;
            }
            let p1 = repo_pkgs[i];
            let p2 = repo_pkgs[j];
            if p1.name() != p2.name() {
                continue;
            }
            let p1_official = p1.db().map(|d| is_arch_repo(d.name())).unwrap_or(false);
            let p2_official = p2.db().map(|d| is_arch_repo(d.name())).unwrap_or(false);

            if p1_official && !p2_official {
                let cmp = alpm::vercmp(p1.version().as_str(), p2.version().as_str());
                if cmp != std::cmp::Ordering::Less {
                    remove_repo_indices.insert(j);
                } else {
                    remove_repo_indices.insert(i);
                }
            }
        }
    }

    if !remove_repo_indices.is_empty() {
        let mut idx = 0;
        repo_pkgs.retain(|_| {
            let keep = !remove_repo_indices.contains(&idx);
            idx += 1;
            keep
        });
    }

    // 2. Deduplicate non-official sync repo vs AUR packages
    let mut remove_custom_indices = std::collections::HashSet::new();
    let mut remove_aur_names = std::collections::HashSet::new();

    for (i, repo_pkg) in repo_pkgs.iter().enumerate() {
        let is_official = repo_pkg.db().map(|d| is_arch_repo(d.name())).unwrap_or(false);
        if is_official {
            continue;
        }

        if let Some(aur_pkg) = aur_pkgs.iter().find(|a| a.name == repo_pkg.name()) {
            let cmp = alpm::vercmp(repo_pkg.version().as_str(), aur_pkg.version.as_str());
            if cmp != std::cmp::Ordering::Less {
                // MKVPKG version is >= AUR version: remove duplicate aur_pkg from aur_pkgs
                remove_aur_names.insert(aur_pkg.name.clone());
            } else {
                // MKVPKG version is strictly older than AUR version: remove stale MKVPKG entry from repo_pkgs
                remove_custom_indices.insert(i);
            }
        }
    }

    if !remove_custom_indices.is_empty() {
        let mut idx = 0;
        repo_pkgs.retain(|_| {
            let keep = !remove_custom_indices.contains(&idx);
            idx += 1;
            keep
        });
    }

    if !remove_aur_names.is_empty() {
        aur_pkgs.retain(|a| !remove_aur_names.contains(&a.name));
    }
}

#[cfg(test)]
pub fn deduplicate_entries(
    repos: &mut Vec<(&str, &str, &str)>, // (name, version, db_name)
    aur: &mut Vec<(&str, &str)>,          // (name, version)
) {
    let mut remove_repo_indices = std::collections::HashSet::new();
    for i in 0..repos.len() {
        for j in 0..repos.len() {
            if i == j {
                continue;
            }
            let (name1, ver1, db1) = repos[i];
            let (name2, ver2, db2) = repos[j];
            if name1 != name2 {
                continue;
            }
            let p1_official = is_arch_repo(db1);
            let p2_official = is_arch_repo(db2);

            if p1_official && !p2_official {
                let cmp = alpm::vercmp(ver1, ver2);
                if cmp != std::cmp::Ordering::Less {
                    remove_repo_indices.insert(j);
                } else {
                    remove_repo_indices.insert(i);
                }
            }
        }
    }

    if !remove_repo_indices.is_empty() {
        let mut idx = 0;
        repos.retain(|_| {
            let keep = !remove_repo_indices.contains(&idx);
            idx += 1;
            keep
        });
    }

    let mut remove_custom_indices = std::collections::HashSet::new();
    let mut remove_aur_names = std::collections::HashSet::new();

    for (i, &(repo_name, repo_ver, db_name)) in repos.iter().enumerate() {
        let is_official = is_arch_repo(db_name);
        if is_official {
            continue;
        }

        if let Some(&(_, aur_ver)) = aur.iter().find(|(name, _)| *name == repo_name) {
            let cmp = alpm::vercmp(repo_ver, aur_ver);
            if cmp != std::cmp::Ordering::Less {
                remove_aur_names.insert(repo_name);
            } else {
                remove_custom_indices.insert(i);
            }
        }
    }

    if !remove_custom_indices.is_empty() {
        let mut idx = 0;
        repos.retain(|_| {
            let keep = !remove_custom_indices.contains(&idx);
            idx += 1;
            keep
        });
    }

    if !remove_aur_names.is_empty() {
        aur.retain(|(name, _)| !remove_aur_names.contains(name));
    }
}

#[cfg(test)]
mod deduplication_tests {
    use super::*;

    #[test]
    fn test_deduplicate_official_vs_custom() {
        // Official is equal to Custom -> custom is filtered out
        let mut repos = vec![
            ("ripgrep", "14.1.0-1", "extra"),
            ("ripgrep", "14.1.0-1", "MKVPKG"),
        ];
        let mut aur = vec![];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("ripgrep", "14.1.0-1", "extra")]);

        // Official is newer than Custom -> custom is filtered out
        let mut repos = vec![
            ("ripgrep", "14.2.0-1", "extra"),
            ("ripgrep", "14.1.0-1", "MKVPKG"),
        ];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("ripgrep", "14.2.0-1", "extra")]);

        // Custom is strictly newer than Official -> official is filtered out
        let mut repos = vec![
            ("ripgrep", "14.1.0-1", "extra"),
            ("ripgrep", "14.1.1-1", "MKVPKG"),
        ];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("ripgrep", "14.1.1-1", "MKVPKG")]);
    }

    #[test]
    fn test_deduplicate_custom_vs_aur() {
        // Custom is equal to AUR -> duplicate AUR is filtered out, MKVPKG kept
        let mut repos = vec![("custom-tool", "1.0.0-1", "MKVPKG")];
        let mut aur = vec![("custom-tool", "1.0.0-1")];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("custom-tool", "1.0.0-1", "MKVPKG")]);
        assert!(aur.is_empty());

        // Custom is newer than AUR (e.g. locally built -git) -> AUR filtered out, MKVPKG kept
        let mut repos = vec![("custom-tool", "2.0.0-1", "MKVPKG")];
        let mut aur = vec![("custom-tool", "1.0.0-1")];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("custom-tool", "2.0.0-1", "MKVPKG")]);
        assert!(aur.is_empty());

        // Custom is strictly older than AUR -> stale MKVPKG filtered out, AUR kept
        let mut repos = vec![("custom-tool", "1.0.0-1", "MKVPKG")];
        let mut aur = vec![("custom-tool", "1.0.1-1")];
        deduplicate_entries(&mut repos, &mut aur);
        assert!(repos.is_empty());
        assert_eq!(aur, vec![("custom-tool", "1.0.1-1")]);
    }

    #[test]
    fn test_deduplicate_official_vs_aur_preserved() {
        // Official repos vs AUR packages are untouched by deduplicate_entries
        let mut repos = vec![("git", "2.40.0-1", "extra")];
        let mut aur = vec![("git", "2.40.0-1")];
        deduplicate_entries(&mut repos, &mut aur);
        assert_eq!(repos, vec![("git", "2.40.0-1", "extra")]);
        assert_eq!(aur, vec![("git", "2.40.0-1")]);
    }
}

#[cfg(all(test, feature = "mock"))]
mod release_age_tests {
    use super::search_aur;
    use crate::config::{Config, Mode, SortBy};
    use crate::mock::Mock;
    use crate::util::now_secs;
    use globset::{Glob, GlobSetBuilder};
    use raur::Package;
    use std::time::Duration;

    #[tokio::test]
    async fn age_filter_precedes_limit_and_preserves_exemptions() {
        let now = now_secs();
        let mut config = Config::default();
        config.mode = Mode::AUR;
        config.sort_by = SortBy::Name;
        config.limit = 1;
        config.min_release_age = Some(Duration::from_secs(86_400));
        config.devel_suffixes = vec!["-git".into()];
        config.min_release_age_exclude = GlobSetBuilder::new()
            .add(Glob::new("pkg-d-*").unwrap())
            .build()
            .unwrap();
        config.raur = Some(Mock::with_packages(
            [
                ("pkg-a-new", now),
                ("pkg-b-old", now - 2 * 86_400),
                ("pkg-c-git", now),
                ("pkg-d-excluded", now),
            ]
            .into_iter()
            .map(|(name, last_modified)| Package {
                name: name.into(),
                last_modified,
                ..Package::default()
            }),
        ));
        let query = ["pkg".into()];
        let matches = search_aur(&config, &query).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "pkg-b-old");

        config.limit = 0;
        let matches = search_aur(&config, &query).await.unwrap();
        let names = matches
            .iter()
            .map(|pkg| pkg.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["pkg-b-old", "pkg-c-git", "pkg-d-excluded"]);

        config.min_release_age = None;
        let matches = search_aur(&config, &query).await.unwrap();
        assert_eq!(matches.len(), 4);
        assert_eq!(matches[0].name, "pkg-a-new");
    }
}
