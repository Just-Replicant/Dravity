//! File selection on the command line: `--files 0,2-4` and
//! `--file-priority 1=high`.

use dravity_engine::Priority;

/// File indices given with `--files` (one argument, so clap keeps it whole).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileList(pub(crate) Vec<usize>);

/// Parses `--files`.
pub(crate) fn parse_file_list(s: &str) -> Result<FileList, String> {
    parse_indices(s).map(FileList)
}

/// Parses a list of file indices and inclusive ranges: `0,2-4,7`.
pub(crate) fn parse_indices(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || format!("invalid file index or range {part:?} (expected e.g. 0,2-4)");
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.trim().parse().map_err(|_| bad())?;
                let b: usize = b.trim().parse().map_err(|_| bad())?;
                if a > b || b - a >= 1_000_000 {
                    return Err(bad());
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse().map_err(|_| bad())?),
        }
    }
    if out.is_empty() {
        return Err("no file indices given".to_owned());
    }
    Ok(out)
}

/// Parses `INDEX=PRIORITY`, e.g. `3=high`.
pub(crate) fn parse_file_priority(s: &str) -> Result<(usize, Priority), String> {
    let (index, priority) = s
        .split_once('=')
        .ok_or_else(|| format!("expected INDEX=PRIORITY, got {s:?}"))?;
    let index = index
        .trim()
        .parse()
        .map_err(|_| format!("invalid file index {index:?}"))?;
    let priority = priority.trim().parse().map_err(|e| format!("{e}"))?;
    Ok((index, priority))
}

/// One priority per file: with `only`, unlisted files are skipped; then
/// `overrides` apply. Empty when nothing was asked (all normal).
pub(crate) fn file_priorities(
    count: usize,
    only: Option<&[usize]>,
    overrides: &[(usize, Priority)],
) -> Result<Vec<Priority>, String> {
    if only.is_none() && overrides.is_empty() {
        return Ok(Vec::new());
    }
    let check = |index: usize| {
        if index < count {
            Ok(index)
        } else {
            Err(format!(
                "file index {index} out of range: the torrent has {count} file{} (see `dravity info`)",
                if count == 1 { "" } else { "s" }
            ))
        }
    };
    let mut priorities = vec![
        if only.is_some() {
            Priority::Skip
        } else {
            Priority::Normal
        };
        count
    ];
    for &index in only.unwrap_or_default() {
        if let Some(p) = priorities.get_mut(check(index)?) {
            *p = Priority::Normal;
        }
    }
    for &(index, priority) in overrides {
        if let Some(p) = priorities.get_mut(check(index)?) {
            *p = priority;
        }
    }
    Ok(priorities)
}

#[cfg(test)]
mod tests {
    use dravity_engine::Priority::{High, Low, Normal, Skip};

    use super::{file_priorities, parse_file_priority, parse_indices};

    #[test]
    fn indices_and_ranges() {
        assert_eq!(parse_indices("0,2-4, 7"), Ok(vec![0, 2, 3, 4, 7]));
        assert_eq!(parse_indices("3"), Ok(vec![3]));
        assert!(parse_indices("").is_err());
        assert!(parse_indices("4-2").is_err());
        assert!(parse_indices("a").is_err());
        assert!(parse_indices("0-99999999").is_err());
    }

    #[test]
    fn index_priority_pairs() {
        assert_eq!(parse_file_priority("3=high"), Ok((3, High)));
        assert_eq!(parse_file_priority(" 0 = skip "), Ok((0, Skip)));
        assert!(parse_file_priority("3").is_err());
        assert!(parse_file_priority("x=high").is_err());
        assert!(parse_file_priority("1=urgent").is_err());
    }

    #[test]
    fn selection_skips_the_rest_and_overrides_win() {
        assert_eq!(file_priorities(3, None, &[]), Ok(vec![]));
        assert_eq!(
            file_priorities(4, Some(&[1, 3]), &[]),
            Ok(vec![Skip, Normal, Skip, Normal])
        );
        assert_eq!(
            file_priorities(3, Some(&[0]), &[(0, High), (2, Low)]),
            Ok(vec![High, Skip, Low])
        );
        assert_eq!(
            file_priorities(2, None, &[(1, Skip)]),
            Ok(vec![Normal, Skip])
        );
        let err = file_priorities(2, Some(&[2]), &[]).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }
}
