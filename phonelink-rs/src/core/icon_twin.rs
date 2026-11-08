//! A resizable twin of a fixed-size icon theme.
//!
//! Breeze lists its folders as `Type=Fixed` (16, 22, 24 px and @2x/@3x
//! copies), and its 22/24 px drawings are the 16 px glyph with a wide
//! margin, so a bigger icon size picked a bigger folder but drew about the
//! same glyph: icons never grew with Settings > Size (Ethan's laptop,
//! 2026-11-08). The twin points at the 16 px folders and calls them
//! scalable, so GTK draws those SVGs at the size asked for. It inherits
//! the real theme, so anything it lacks still comes from there.

use std::collections::BTreeMap;

/// One folder of an `index.theme`.
#[derive(Debug, Clone, PartialEq)]
struct Dir {
    path: String,
    size: u32,
    scale: u32,
    context: String,
    scalable: bool,
}

fn parse(index: &str) -> Vec<Dir> {
    let mut out = Vec::new();
    let mut cur: Option<(String, BTreeMap<String, String>)> = None;
    let flush = |cur: &mut Option<(String, BTreeMap<String, String>)>, out: &mut Vec<Dir>| {
        if let Some((path, keys)) = cur.take()
            && path != "Icon Theme"
            && let Some(size) = keys.get("Size").and_then(|s| s.trim().parse().ok())
        {
            out.push(Dir {
                path,
                size,
                scale: keys.get("Scale").and_then(|s| s.trim().parse().ok()).unwrap_or(1),
                context: keys.get("Context").cloned().unwrap_or_default(),
                scalable: keys.get("Type").is_some_and(|t| t.trim() == "Scalable"),
            });
        }
    };
    for line in index.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            flush(&mut cur, &mut out);
            cur = Some((name.to_string(), BTreeMap::new()));
        } else if let (Some((_, keys)), Some((k, v))) = (cur.as_mut(), line.split_once('=')) {
            keys.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// The twin of theme `name` (its `index.theme` text): the twin's
/// `index.theme` and the folders to link (link name, folder in the theme).
/// None when the theme already scales (Adwaita) or has nothing to twin.
pub fn twin(index: &str, name: &str) -> Option<(String, Vec<(String, String)>)> {
    let dirs: Vec<Dir> = parse(index).into_iter().filter(|d| d.scale == 1).collect();
    // Scalable folders for the small, everyday icons: nothing to fix.
    // (Breeze's are 32 px and up, plus a few odd ones like applets/22.)
    let everyday = |d: &Dir| d.context == "Actions" && d.size <= 24;
    if dirs.iter().any(|d| d.scalable && everyday(d)) {
        return None;
    }
    let dirs: Vec<Dir> = dirs.into_iter().filter(|d| !d.scalable).collect();
    if dirs.is_empty() {
        return None;
    }
    // Per kind of icon, the 16 px drawings, at every size: Breeze's 22 and
    // 24 px drawings are the same glyph with a wide margin (its bell fills
    // 14 of 24 px), so scaling those up hardly grows the icon.
    let mut by_context: BTreeMap<&str, Vec<&Dir>> = BTreeMap::new();
    for d in &dirs {
        by_context.entry(d.context.as_str()).or_default().push(d);
    }
    let mut links = Vec::new();
    let mut sections = String::new();
    for (context, ds) in by_context {
        let Some(d) = ds.iter().find(|d| d.size == 16).or_else(|| ds.iter().min_by_key(|d| d.size)) else {
            continue;
        };
        let tag: String = context.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
        let tag = if tag.is_empty() { "other".to_string() } else { tag };
        let link = format!("{tag}-{}", d.size);
        sections.push_str(&format!(
            "\n[{link}]\nSize={}\nContext={context}\nType=Scalable\nMinSize=8\nMaxSize=512\n",
            d.size
        ));
        links.push((link, d.path.clone()));
    }
    if links.is_empty() {
        return None;
    }
    let names: Vec<&str> = links.iter().map(|(l, _)| l.as_str()).collect();
    let head = format!(
        "[Icon Theme]\nName=PhoneRelay {name}\nComment=Resizable view of {name}\nInherits={name},hicolor\nDirectories={}\n",
        names.join(",")
    );
    Some((head + &sections, links))
}

/// The twin's theme name for `name`.
pub fn twin_name(name: &str) -> String {
    format!("phonelink-{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BREEZE: &str = "[Icon Theme]\nName=Breeze Dark\nDirectories=actions/16,actions/16@2x,actions/22,actions/24,status/16\n\n\
        #16x16 - Fixed size\n[actions/16]\nSize=16\nContext=Actions\nType=Fixed\n\n\
        [actions/16@2x]\nSize=16\nScale=2\nContext=Actions\nType=Fixed\n\n\
        [actions/22]\nSize=22\nContext=Actions\nType=Fixed\n\n\
        [actions/24]\nSize=24\nContext=Actions\nType=Fixed\n\n\
        [status/16]\nSize=16\nContext=Status\nType=Fixed\n\n\
        [applets/22]\nSize=22\nContext=Status\nType=Scalable\nMinSize=22\nMaxSize=22\n\n\
        [actions/32]\nSize=32\nContext=Actions\nType=Scalable\nMinSize=32\nMaxSize=256\n";

    #[test]
    fn a_fixed_theme_gets_a_resizable_twin() {
        let (index, links) = twin(BREEZE, "breeze-dark").unwrap();
        assert_eq!(
            links,
            vec![
                ("actions-16".to_string(), "actions/16".to_string()),
                ("status-16".to_string(), "status/16".to_string()),
            ]
        );
        assert!(index.contains("Inherits=breeze-dark,hicolor"));
        assert!(index.contains("Directories=actions-16,status-16"));
        assert!(index.contains("[actions-16]\nSize=16\nContext=Actions\nType=Scalable\nMinSize=8\nMaxSize=512"));
        assert!(index.contains("[status-16]\nSize=16\nContext=Status\nType=Scalable\nMinSize=8\nMaxSize=512"));
    }

    #[test]
    fn a_theme_that_scales_is_left_alone() {
        let adwaita = "[Icon Theme]\nName=Adwaita\n[symbolic/actions]\nSize=16\nContext=Actions\nType=Scalable\n";
        assert_eq!(twin(adwaita, "Adwaita"), None);
        assert_eq!(twin("[Icon Theme]\nName=x\n", "x"), None);
    }
}
