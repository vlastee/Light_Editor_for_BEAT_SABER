//! The map browser (Ctrl+O): lists the maps in a folder with their covers, song, artist, mapper,
//! BPM and environment, with search, sorting, folder navigation and shortcuts to the Beat Saber
//! song folders found on this computer. Folders are read and covers decoded on a worker thread.

use eframe::egui;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

/// Cover thumbnails are this many pixels square.
const THUMB: usize = 64;
const ROW_HEIGHT: f32 = 52.0;

/// A map folder (one with an Info.dat) and what its Info.dat says.
#[derive(Clone, Debug, Default)]
pub struct MapInfo {
    pub song: String,
    pub sub_name: String,
    pub artist: String,
    pub mapper: String,
    pub bpm: f64,
    pub environment: String,
    pub difficulties: usize,
    pub cover: Option<PathBuf>,
    pub modified: u64,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// `None` for a plain folder to open.
    pub map: Option<MapInfo>,
}

/// Reads a map's Info.dat (v2 or v4).
pub fn read_info(folder: &Path) -> Option<MapInfo> {
    let file = ["Info.dat", "info.dat"].iter().map(|n| folder.join(n)).find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&file).ok()?;
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let s = |x: &Value| x.as_str().unwrap_or_default().trim().to_owned();
    let modified = std::fs::metadata(folder)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut info = if v.get("_version").is_some() || v.get("_songName").is_some() {
        MapInfo {
            song: s(&v["_songName"]),
            sub_name: s(&v["_songSubName"]),
            artist: s(&v["_songAuthorName"]),
            mapper: s(&v["_levelAuthorName"]),
            bpm: v["_beatsPerMinute"].as_f64().unwrap_or(0.0),
            environment: s(&v["_environmentName"]),
            difficulties: v["_difficultyBeatmapSets"].as_array().into_iter().flatten().map(|set| set["_difficultyBeatmaps"].as_array().map_or(0, Vec::len)).sum(),
            cover: v["_coverImageFilename"].as_str().filter(|c| !c.is_empty()).map(|c| folder.join(c)),
            modified,
        }
    } else {
        let mappers: Vec<String> = v["difficultyBeatmaps"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|d| d["beatmapAuthors"]["mappers"].as_array().cloned().unwrap_or_default())
            .filter_map(|m| m.as_str().map(str::to_owned))
            .fold(Vec::new(), |mut acc, m| {
                if !acc.contains(&m) {
                    acc.push(m);
                }
                acc
            });
        MapInfo {
            song: s(&v["song"]["title"]),
            sub_name: s(&v["song"]["subTitle"]),
            artist: s(&v["song"]["author"]),
            mapper: mappers.join(", "),
            bpm: v["audio"]["bpm"].as_f64().unwrap_or(0.0),
            environment: v["environmentNames"].as_array().and_then(|a| a.first()).map(s).unwrap_or_default(),
            difficulties: v["difficultyBeatmaps"].as_array().map_or(0, Vec::len),
            cover: v["coverImageFilename"].as_str().filter(|c| !c.is_empty()).map(|c| folder.join(c)),
            modified,
        }
    };
    if info.song.is_empty() {
        info.song = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    }
    info.environment = info.environment.trim_end_matches("Environment").to_owned();
    Some(info)
}

/// The folder's subfolders: maps first-class, plain folders to step into. Sorted by name.
pub fn scan(dir: &Path) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let path = e.path();
            Entry { name: e.file_name().to_string_lossy().into_owned(), map: read_info(&path), path }
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

/// Beat Saber song folders on this computer: BSManager instances, Steam libraries and the
/// folder above the last opened map.
pub fn song_folders(last_map: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |label: String, p: PathBuf| {
        if p.is_dir() && !out.iter().any(|(_, q)| q == &p) {
            out.push((label, p));
        }
    };
    if let Some(parent) = last_map.and_then(Path::parent) {
        add("Last used".to_owned(), parent.to_path_buf());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let instances = home.join(".local/share/BSManager/BSInstances");
    if let Ok(read) = std::fs::read_dir(&instances) {
        let mut names: Vec<_> = read.filter_map(Result::ok).map(|e| e.path()).collect();
        names.sort();
        for p in names {
            let v = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            add(format!("BSManager {v}"), p.join("Beat Saber_Data/CustomLevels"));
            add(format!("BSManager {v} WIP"), p.join("Beat Saber_Data/CustomWIPLevels"));
        }
    }
    add("BSManager shared".to_owned(), home.join(".local/share/BSManager/SharedContent/SharedMaps/CustomLevels"));
    for steam in [home.join(".local/share/Steam"), home.join(".steam/steam")] {
        add("Steam".to_owned(), steam.join("steamapps/common/Beat Saber/Beat Saber_Data/CustomLevels"));
        add("Steam WIP".to_owned(), steam.join("steamapps/common/Beat Saber/Beat Saber_Data/CustomWIPLevels"));
    }
    out
}

/// A cover image (PNG or JPEG) shrunk to a THUMB×THUMB RGBA thumbnail.
fn thumbnail(path: &Path) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    let (w, h, rgba) = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        let mut dec = png::Decoder::new(std::io::Cursor::new(&data));
        dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = dec.read_info().ok()?;
        let mut buf = vec![0; reader.output_buffer_size()?];
        let info = reader.next_frame(&mut buf).ok()?;
        let n = info.color_type.samples();
        let px = (info.width * info.height) as usize;
        let rgba: Vec<u8> = (0..px)
            .flat_map(|i| {
                let p = &buf[i * n..i * n + n];
                match n {
                    1 => [p[0], p[0], p[0], 255],
                    2 => [p[0], p[0], p[0], p[1]],
                    3 => [p[0], p[1], p[2], 255],
                    _ => [p[0], p[1], p[2], p[3]],
                }
            })
            .collect();
        (info.width as usize, info.height as usize, rgba)
    } else {
        let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&data));
        let pixels = dec.decode().ok()?;
        let info = dec.info()?;
        let (w, h) = (info.width as usize, info.height as usize);
        let n = pixels.len() / (w * h).max(1);
        if n == 0 {
            return None;
        }
        let rgba = (0..w * h)
            .flat_map(|i| {
                let p = &pixels[i * n..i * n + n];
                if n >= 3 { [p[0], p[1], p[2], 255] } else { [p[0], p[0], p[0], 255] }
            })
            .collect();
        (w, h, rgba)
    };
    if w == 0 || h == 0 {
        return None;
    }
    // Box filter down to the thumbnail.
    let mut out = vec![0u8; THUMB * THUMB * 4];
    for ty in 0..THUMB {
        for tx in 0..THUMB {
            let (x0, x1) = (tx * w / THUMB, ((tx + 1) * w / THUMB).max(tx * w / THUMB + 1).min(w));
            let (y0, y1) = (ty * h / THUMB, ((ty + 1) * h / THUMB).max(ty * h / THUMB + 1).min(h));
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    (0..4).for_each(|k| sum[k] += p[k] as u32);
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1) as u32;
            (0..4).for_each(|k| out[(ty * THUMB + tx) * 4 + k] = (sum[k] / n) as u8);
        }
    }
    Some(out)
}

enum Job {
    Scan(PathBuf),
    Cover(PathBuf),
}

enum Done {
    Scanned(PathBuf, Result<Vec<Entry>, String>),
    Cover(PathBuf, Option<Vec<u8>>),
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Artist,
    Mapper,
    Newest,
}

pub struct MapBrowser {
    pub open: bool,
    dir: PathBuf,
    dir_input: String,
    entries: Vec<Entry>,
    loading: bool,
    error: Option<String>,
    search: String,
    sort: Sort,
    selected: Option<PathBuf>,
    places: Vec<(String, PathBuf)>,
    covers: HashMap<PathBuf, Option<egui::TextureHandle>>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
}

impl MapBrowser {
    pub fn new() -> MapBrowser {
        let (jobs, rx) = channel::<Job>();
        let (tx, done) = channel::<Done>();
        std::thread::Builder::new()
            .name("map browser".into())
            .spawn(move || {
                // Newest scan wins; covers queue behind it.
                while let Ok(job) = rx.recv() {
                    let msg = match job {
                        Job::Scan(dir) => Done::Scanned(dir.clone(), scan(&dir)),
                        Job::Cover(path) => Done::Cover(path.clone(), thumbnail(&path)),
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            })
            .ok();
        MapBrowser {
            open: false,
            dir: PathBuf::new(),
            dir_input: String::new(),
            entries: Vec::new(),
            loading: false,
            error: None,
            search: String::new(),
            sort: Sort::Name,
            selected: None,
            places: Vec::new(),
            covers: HashMap::new(),
            jobs,
            done,
        }
    }

    /// Opens the browser at the folder holding `current` (the open map), else the first song
    /// folder found.
    pub fn show(&mut self, current: Option<&Path>) {
        self.open = true;
        self.places = song_folders(current);
        self.selected = current.map(Path::to_path_buf);
        if self.dir.as_os_str().is_empty() || !self.dir.is_dir() {
            let start = current.and_then(Path::parent).map(Path::to_path_buf).or_else(|| self.places.first().map(|p| p.1.clone()));
            if let Some(d) = start {
                self.go(d);
            }
        } else {
            let d = self.dir.clone();
            self.go(d);
        }
    }

    fn go(&mut self, dir: PathBuf) {
        self.dir_input = dir.display().to_string();
        self.dir = dir.clone();
        self.loading = true;
        self.error = None;
        let _ = self.jobs.send(Job::Scan(dir));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.done.try_recv() {
            match msg {
                Done::Scanned(dir, result) if dir == self.dir => {
                    self.loading = false;
                    match result {
                        Ok(entries) => self.entries = entries,
                        Err(e) => {
                            self.entries.clear();
                            self.error = Some(e);
                        }
                    }
                }
                Done::Scanned(..) => {}
                Done::Cover(path, rgba) => {
                    let tex = rgba.map(|px| {
                        let img = egui::ColorImage::from_rgba_unmultiplied([THUMB, THUMB], &px);
                        ctx.load_texture(path.display().to_string(), img, egui::TextureOptions::LINEAR)
                    });
                    self.covers.insert(path, tex);
                }
            }
        }
    }

    /// The indices of the entries shown: filtered by the search words, folders first, sorted.
    fn visible(&self) -> Vec<usize> {
        let words: Vec<String> = self.search.to_lowercase().split_whitespace().map(str::to_owned).collect();
        let mut list: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                let hay = match &e.map {
                    Some(m) => format!("{} {} {} {} {} {}", e.name, m.song, m.sub_name, m.artist, m.mapper, m.environment).to_lowercase(),
                    None => e.name.to_lowercase(),
                };
                words.iter().all(|w| hay.contains(w))
            })
            .collect();
        let key = |e: &Entry| -> String {
            let m = e.map.as_ref();
            match self.sort {
                Sort::Name | Sort::Newest => m.map_or(e.name.clone(), |m| m.song.clone()),
                Sort::Artist => m.map_or(String::new(), |m| m.artist.clone()),
                Sort::Mapper => m.map_or(String::new(), |m| m.mapper.clone()),
            }
            .to_lowercase()
        };
        list.sort_by(|&a, &b| {
            let (a, b) = (&self.entries[a], &self.entries[b]);
            a.map.is_some().cmp(&b.map.is_some()).then_with(|| match self.sort {
                Sort::Newest => b.map.as_ref().map_or(0, |m| m.modified).cmp(&a.map.as_ref().map_or(0, |m| m.modified)),
                _ => key(a).cmp(&key(b)),
            })
        });
        list
    }

    /// Draws the browser window; returns the map folder to open.
    pub fn ui(&mut self, ctx: &egui::Context, inputs: &mut Vec<egui::Id>) -> Option<PathBuf> {
        if !self.open {
            return None;
        }
        self.poll(ctx);
        let mut pick = None;
        let mut go = None;
        let mut open = self.open;
        egui::Window::new("Open map")
            .open(&mut open)
            .default_size([760.0, 560.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("⬆").on_hover_text("Parent folder").clicked() {
                        go = self.dir.parent().map(Path::to_path_buf);
                    }
                    let r = ui.add(egui::TextEdit::singleline(&mut self.dir_input).desired_width(ui.available_width() - 60.0));
                    inputs.push(r.id);
                    if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || ui.button("Go").clicked() {
                        go = Some(PathBuf::from(self.dir_input.trim()));
                    }
                });
                if !self.places.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (label, p) in &self.places {
                            if ui.selectable_label(*p == self.dir, label).on_hover_text(p.display().to_string()).clicked() {
                                go = Some(p.clone());
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search song, artist, mapper, environment").desired_width(320.0));
                    inputs.push(r.id);
                    if self.search.is_empty() && !ui.ctx().memory(|m| m.focused().is_some()) {
                        r.request_focus();
                    }
                    ui.label("Sort");
                    for (s, label) in [(Sort::Name, "Song"), (Sort::Artist, "Artist"), (Sort::Mapper, "Mapper"), (Sort::Newest, "Newest")] {
                        ui.selectable_value(&mut self.sort, s, label);
                    }
                });
                ui.separator();
                let list = self.visible();
                let entries = &self.entries;
                let maps = list.iter().filter(|&&i| entries[i].map.is_some()).count();
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
                } else if self.loading {
                    ui.label("Reading folder…");
                } else if self.entries.is_empty() {
                    ui.label("No folders here.");
                } else if list.is_empty() {
                    ui.label("Nothing matches the search.");
                }
                let mut wanted = Vec::new();
                let total = list.len();
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 30.0).show_rows(ui, ROW_HEIGHT, total, |ui, range| {
                    for &i in &list[range] {
                        let e = &entries[i];
                        let selected = self.selected.as_deref() == Some(e.path.as_path());
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT - 2.0), egui::Sense::click());
                        let visuals = ui.visuals();
                        if selected {
                            ui.painter().rect_filled(rect, 3.0, visuals.selection.bg_fill);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
                        }
                        let text = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
                        let weak = visuals.weak_text_color();
                        let cover_rect = egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 3.0), egui::vec2(ROW_HEIGHT - 8.0, ROW_HEIGHT - 8.0));
                        let x = cover_rect.max.x + 10.0;
                        let painter = ui.painter_at(rect);
                        match &e.map {
                            Some(m) => {
                                match m.cover.as_ref().map(|c| (c, self.covers.get(c))) {
                                    Some((_, Some(Some(tex)))) => {
                                        painter.image(tex.id(), cover_rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                                    }
                                    other => {
                                        painter.rect_filled(cover_rect, 3.0, visuals.extreme_bg_color);
                                        if let Some((c, None)) = other {
                                            wanted.push(c.clone());
                                        }
                                    }
                                }
                                let title = if m.sub_name.is_empty() { m.song.clone() } else { format!("{} {}", m.song, m.sub_name) };
                                painter.text(egui::pos2(x, rect.min.y + 6.0), egui::Align2::LEFT_TOP, format!("{title}  —  {}", m.artist), egui::FontId::proportional(15.0), text);
                                let mut details = vec![format!("mapped by {}", if m.mapper.is_empty() { "?" } else { &m.mapper })];
                                if m.bpm > 0.0 {
                                    details.push(format!("{} BPM", (m.bpm * 100.0).round() / 100.0));
                                }
                                if !m.environment.is_empty() {
                                    details.push(m.environment.clone());
                                }
                                details.push(format!("{} difficult{}", m.difficulties, if m.difficulties == 1 { "y" } else { "ies" }));
                                painter.text(egui::pos2(x, rect.min.y + 27.0), egui::Align2::LEFT_TOP, details.join("  ·  "), egui::FontId::proportional(12.5), weak);
                            }
                            None => {
                                painter.text(cover_rect.center(), egui::Align2::CENTER_CENTER, "📁", egui::FontId::proportional(24.0), text);
                                painter.text(egui::pos2(x, rect.center().y), egui::Align2::LEFT_CENTER, &e.name, egui::FontId::proportional(15.0), text);
                            }
                        }
                        let resp = resp.on_hover_text(e.path.display().to_string());
                        if resp.clicked() {
                            self.selected = Some(e.path.clone());
                        }
                        if resp.double_clicked() {
                            if e.map.is_some() {
                                pick = Some(e.path.clone());
                            } else {
                                go = Some(e.path.clone());
                            }
                        }
                    }
                });
                // Covers for the rows on screen, a few per frame.
                for c in wanted.into_iter().take(8) {
                    self.covers.insert(c.clone(), None);
                    let _ = self.jobs.send(Job::Cover(c));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(format!("{maps} maps{}", if total > maps { format!(", {} folders", total - maps) } else { String::new() }));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let chosen = self.selected.clone().filter(|p| self.entries.iter().any(|e| &e.path == p));
                        let is_map = chosen.as_ref().is_some_and(|p| self.entries.iter().any(|e| &e.path == p && e.map.is_some()));
                        if ui.add_enabled(chosen.is_some(), egui::Button::new(if is_map || chosen.is_none() { "Open" } else { "Open folder" })).clicked() {
                            if is_map {
                                pick = chosen;
                            } else {
                                go = chosen;
                            }
                        }
                    });
                });
                // Enter opens the selection; the only search match opens on Enter too.
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && pick.is_none() && go.is_none() {
                    let only = (maps == 1).then(|| list.iter().map(|&i| &entries[i]).find(|e| e.map.is_some()).map(|e| e.path.clone())).flatten();
                    let shown = self.selected.clone().filter(|p| list.iter().any(|&i| &entries[i].path == p && entries[i].map.is_some()));
                    pick = shown.or(only);
                }
            });
        if self.loading {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if self.covers.values().any(Option::is_none) {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
        }
        self.open = open;
        if let Some(d) = go {
            self.search.clear();
            self.go(d);
        }
        if pick.is_some() {
            self.open = false;
        }
        pick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_v2_and_v4_info() {
        let dir = std::env::temp_dir().join(format!("bslt-browser-{}", std::process::id()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("Info.dat"), r#"{"_version":"2.1.0","_songName":"Song","_songAuthorName":"Artist","_levelAuthorName":"Me","_beatsPerMinute":128,"_environmentName":"BigMirrorEnvironment","_coverImageFilename":"cover.jpg","_difficultyBeatmapSets":[{"_difficultyBeatmaps":[{},{}]}]}"#).unwrap();
        std::fs::write(b.join("Info.dat"), r#"{"version":"4.0.0","song":{"title":"Four","author":"X"},"audio":{"bpm":90},"environmentNames":["WeaveEnvironment"],"difficultyBeatmaps":[{"beatmapAuthors":{"mappers":["A","B"]}},{"beatmapAuthors":{"mappers":["A"]}}]}"#).unwrap();
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let list = scan(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(list.len(), 3);
        let m = list[0].map.as_ref().unwrap();
        assert_eq!((m.song.as_str(), m.artist.as_str(), m.mapper.as_str(), m.difficulties, m.environment.as_str()), ("Song", "Artist", "Me", 2, "BigMirror"));
        assert_eq!(m.cover.as_ref().unwrap().file_name().unwrap(), "cover.jpg");
        let v4 = list[1].map.as_ref().unwrap();
        assert_eq!((v4.song.as_str(), v4.mapper.as_str(), v4.bpm, v4.difficulties), ("Four", "A, B", 90.0, 2));
        assert!(list[2].map.is_none());
    }
}
