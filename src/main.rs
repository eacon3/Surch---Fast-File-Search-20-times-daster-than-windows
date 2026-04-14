use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use hashbrown::HashMap as BrownHashMap;
use ignore::WalkBuilder;
use rayon::prelude::*;
use clipboard::ClipboardProvider;
use clipboard::ClipboardContext;
use open::that;
use num_cpus;


struct TrieNode {
    children: HashMap<char, TrieNode>,
    file_paths: Vec<Arc<PathBuf>>,
    folder_paths: Vec<Arc<PathBuf>>,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: HashMap::new(),
            file_paths: Vec::new(),
            folder_paths: Vec::new(),
        }
    }
}

struct FileIndex {
    root: TrieNode,
    files: BrownHashMap<String, Vec<Arc<PathBuf>>>,
    folders: BrownHashMap<String, Vec<Arc<PathBuf>>>,
    file_type_map: BrownHashMap<String, Vec<Arc<PathBuf>>>,
}

impl FileIndex {
    fn new() -> Self {
        Self {
            root: TrieNode::new(),
            files: BrownHashMap::new(),
            folders: BrownHashMap::new(),
            file_type_map: BrownHashMap::new(),
        }
    }

    fn add_file(&mut self, path: PathBuf) {
        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or_else(|| {
            path.to_str().unwrap_or("")
        });
        
        let file_name_lower = file_name.to_lowercase();
        let path_arc = Arc::new(path);
        self.files.entry(file_name_lower.clone()).or_default().push(path_arc.clone());
        
        self.add_to_trie(&file_name_lower, path_arc.clone(), false);

        if let Some(extension) = path_arc.extension().and_then(|e| e.to_str()) {
            self.file_type_map.entry(extension.to_lowercase()).or_default().push(path_arc);
        }
    }

    fn add_folder(&mut self, path: PathBuf) {
        let folder_name = path.file_name().and_then(|f| f.to_str()).unwrap_or_else(|| {
            if let Some(drive) = path.to_str() {
                if drive.len() == 3 && drive.chars().nth(1) == Some(':') && drive.chars().nth(2) == Some('\\') {
                    return &drive[0..2];
                }
            }
            path.to_str().unwrap_or("")
        });
        
        if folder_name == "all_temp" {
            println!("Adding folder: {:?} - Full path: {:?}", folder_name, path);
        }
        let folder_name_lower = folder_name.to_lowercase();
        let path_arc = Arc::new(path);
        self.folders.entry(folder_name_lower.clone()).or_default().push(path_arc.clone());
        
        // Add to trie
        self.add_to_trie(&folder_name_lower, path_arc, true);
    }

    fn add_to_trie(&mut self, name: &str, path: Arc<PathBuf>, is_folder: bool) {
        let mut node = &mut self.root;
        
        for c in name.chars() {
            node = node.children.entry(c).or_insert_with(TrieNode::new);
        }
        
        if is_folder {
            node.folder_paths.push(path);
        } else {
            node.file_paths.push(path);
        }
    }

    fn search(&self, query: &str, min_score: f64) -> Vec<(PathBuf, f64, bool)> {
        let query = query.to_lowercase();
        let mut results = Vec::new();
        
        println!("Searching for: {:?}", query);
        println!("Number of files in index: {}", self.files.len());
        println!("Number of folders in index: {}", self.folders.len());

        if query.is_empty() {
            // Add limited files
            let mut file_count = 0;
            for (_, paths) in &self.files {
                for path in paths {
                    results.push((path.as_ref().clone(), 1.0, false));
                    file_count += 1;
                    if file_count >= 50 { // Limit to 50 files
                        break;
                    }
                }
                if file_count >= 50 {
                    break;
                }
            }
            // Add limited folders
            let mut folder_count = 0;
            for (_, paths) in &self.folders {
                for path in paths {
                    results.push((path.as_ref().clone(), 1.0, true));
                    folder_count += 1;
                    if folder_count >= 10 { // Limit to 10 folders
                        break;
                    }
                }
                if folder_count >= 10 {
                    break;
                }
            }
            return results;
        }

        // Fast exact match first
        println!("Checking exact match for: {:?}", query);
        if let Some(file_paths) = self.files.get(query.as_str()) {
            println!("Found exact match files: {}", file_paths.len());
            for path in file_paths {
                results.push((path.as_ref().clone(), 1.0, false));
            }
        }
        if let Some(folder_paths) = self.folders.get(query.as_str()) {
            println!("Found exact match folders: {}", folder_paths.len());
            for path in folder_paths {
                results.push((path.as_ref().clone(), 1.0, true));
            }
        }

        // Check for drive letter match (e.g., "C" -> "C:")
        if query.len() == 1 && query.chars().next().unwrap().is_alphabetic() {
            let drive_key = format!("{}:", query);
            if let Some(folder_paths) = self.folders.get(drive_key.as_str()) {
                println!("Found drive letter match folders: {}", folder_paths.len());
                for path in folder_paths {
                    results.push((path.as_ref().clone(), 1.0, true));
                }
            }
        }

        if !results.is_empty() {
            println!("Returning exact match results: {}", results.len());
            return results;
        }

        // Check if folder exists in the index
        println!("Checking if folder '{}' exists in index: {:?}", query, self.folders.contains_key(query.as_str()));
        println!("Debug: First 10 folder keys:");
        let mut count = 0;
        for key in self.folders.keys() {
            if count < 10 {
                println!("  {}", key);
                count += 1;
            } else {
                break;
            }
        }

        self.search_trie(&self.root, &query, &mut results, min_score);
        if !results.is_empty() {
            println!("Returning trie search results: {}", results.len());
            results.par_sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            return results;
        }

        let files_vec: Vec<_> = self.files.iter().collect();
        let folders_vec: Vec<_> = self.folders.iter().collect();

        // Parallel search files
        let file_results: Vec<_> = files_vec.par_iter()
            .flat_map(|(file_name, paths)| {
                let score = self.calculate_score(*file_name, &query);
                if score >= min_score {
                    paths.iter().map(|path| (path.as_ref().clone(), score, false)).collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            })
            .collect();

        // Parallel search folders
        let folder_results: Vec<_> = folders_vec.par_iter()
            .flat_map(|(folder_name, paths)| {
                let score = self.calculate_score(*folder_name, &query);
                if score >= min_score {
                    paths.iter().map(|path| (path.as_ref().clone(), score, true)).collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            })
            .collect();

        results.extend(file_results);
        results.extend(folder_results);

        let path = Path::new(&query);
        if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
            let folder_name = file_name.to_lowercase();
            if let Some(folder_paths) = self.folders.get(folder_name.as_str()) {
                println!("Found folder by full path: {}", folder_paths.len());
                for path in folder_paths {
                    results.push((path.as_ref().clone(), 1.0, true));
                }
            }
        }
        
        for (folder_name, paths) in &self.folders {
            if let Some(path) = paths.first() {
                if let Some(path_str) = path.to_str() {
                    if path_str.matches('\\').count() == 1 { 
                        let folder_name_lower = folder_name.to_lowercase();
                        if folder_name_lower.contains(&query) {
                            println!("Found root level directory: {}", folder_name);
                            for path in paths {
                                results.push((path.as_ref().clone(), 1.0, true));
                            }
                        }
                    }
                }
            }
        }

        println!("Returning full search results: {}", results.len());

        // Sort by score (descending)
        results.par_sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        results
    }

    fn search_trie(&self, node: &TrieNode, query: &str, results: &mut Vec<(PathBuf, f64, bool)>, min_score: f64) {
        // Add paths from current node
        for path in &node.file_paths {
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
                let score = self.calculate_score(&file_name.to_lowercase(), query);
                if score >= min_score {
                    results.push((path.as_ref().clone(), score, false));
                }
            }
        }

        for path in &node.folder_paths {
            if let Some(folder_name) = path.file_name().and_then(|f| f.to_str()) {
                let score = self.calculate_score(&folder_name.to_lowercase(), query);
                if score >= min_score {
                    results.push((path.as_ref().clone(), score, true));
                }
            }
        }

        // Recurse into children
        for (c, child) in &node.children {
            // Check if query starts with this character
            if let Some(first_char) = query.chars().next() {
                if first_char == *c {
                    let remaining_query: String = query.chars().skip(1).collect();
                    self.search_trie(child, &remaining_query, results, min_score);
                }
            }
        }
    }

    fn calculate_score(&self, item_name: &str, query: &str) -> f64 {
        if query.is_empty() {
            return 1.0;
        }
        
        if item_name == query {
            return 1.0;
        }

        for c in query.chars() {
            if !item_name.contains(c) {
                return 0.0;
            }
        }

        if item_name.contains(query) {
            let query_len = query.chars().count() as f64;
            let item_len = item_name.chars().count() as f64;
            // Give higher score for exact matches at the beginning of the string
            if item_name.starts_with(query) {
                return 0.9 + (query_len / item_len) * 0.1;
            }
            return query_len / item_len;
        }

        let distance = self.levenshtein_distance(item_name, query);
        let max_len = item_name.chars().count().max(query.chars().count()) as f64;
        if max_len > 0.0 {
            let score = 1.0 - (distance as f64 / max_len);
            return score;
        }

        0.0
    }

    fn levenshtein_distance(&self, s1: &str, s2: &str) -> usize {
        let s1_chars: Vec<char> = s1.chars().collect();
        let s2_chars: Vec<char> = s2.chars().collect();
        let s1_len = s1_chars.len();
        let s2_len = s2_chars.len();

        // Early termination for trivial cases
        if s1_len == 0 {
            return s2_len;
        }
        if s2_len == 0 {
            return s1_len;
        }

        let mut prev = vec![0; s2_len + 1];
        let mut curr = vec![0; s2_len + 1];

        for j in 0..=s2_len {
            prev[j] = j;
        }

        for i in 1..=s1_len {
            curr[0] = i;

            for j in 1..=s2_len {
                let cost = if s1_chars[i-1] == s2_chars[j-1] {
                    0
                } else {
                    1
                };

                curr[j] = std::cmp::min(
                    std::cmp::min(curr[j-1] + 1, prev[j] + 1),
                    prev[j-1] + cost
                );
            }

            std::mem::swap(&mut prev, &mut curr);
        }

        prev[s2_len]
    }
}

use std::collections::VecDeque;

// Enum for click behavior
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClickBehavior {
    CopyPath,    
    OpenFolder,  // Open containing folder
}

#[derive(PartialEq)]
enum SearchMode {
    CompleteSearch,
    ProgressiveSearch, 
}

struct SurchApp {
    index: Arc<Mutex<FileIndex>>,
    search_query: String,
    search_results: Vec<(PathBuf, f64, bool)>,
    selected_category: Option<String>,
    selected_file_type: Option<String>,
    is_indexing: bool,
    indexing_progress: f32,
    last_search_time: Instant,
    search_debounce: Duration,
    current_page: usize,
    results_per_page: usize,
    max_results: usize,
    show_settings: bool,
    search_speed: u32, // 1-10, higher = more CPU usage
    use_parallel_search: bool,
    use_trie_search: bool,
    cache_results: bool,
    cache_size: usize,
    search_cache: VecDeque<(String, Vec<(PathBuf, f64, bool)>)>,
    selected_drives: Vec<String>,
    available_drives: Vec<String>,
    click_behavior: ClickBehavior, 
    show_notification: bool, 
    show_notification_window: bool, 
    notification_message: String, 
    last_indexing_start: Instant, 
    custom_path: Option<PathBuf>, 
    indexing_file_count: Arc<Mutex<usize>>, 
    total_files_to_index: Arc<Mutex<usize>>,
    indexing_completed_time: Option<Instant>,
    search_mode: SearchMode, 
    skipped_paths: Vec<String>,
    new_skipped_path: String, 
    show_skip_paths_window: bool,
    skip_paths_current_page: usize, 
    skip_paths_per_page: usize,
    custom_file_types: Vec<String>,
    new_file_type: String, 
    show_file_types_window: bool, 
    show_about_window: bool,
}

impl Default for SurchApp {
    fn default() -> Self {
        // Get available drives
        let mut available_drives = Vec::new();
        for drive in 'A'..='Z' {
            let drive_path = format!(r"{}:\", drive);
            let path = Path::new(&drive_path);
            if path.exists() {
                available_drives.push(drive.to_string());
            }
        }

        Self {
            index: Arc::new(Mutex::new(FileIndex::new())),
            search_query: String::new(),
            search_results: Vec::new(),
            selected_category: None,
            selected_file_type: None,
            is_indexing: false,
            indexing_progress: 0.0,
            last_search_time: Instant::now(),
            search_debounce: Duration::from_millis(1000),
            current_page: 0,
            results_per_page: 10,
            max_results: 100,
            show_settings: false,
            search_speed: 10, // Default to balanced performance
            use_parallel_search: true,
            use_trie_search: true,
            cache_results: true,
            cache_size: 100,
            search_cache: VecDeque::new(),
            selected_drives: available_drives.clone(), 
            available_drives,
            click_behavior: ClickBehavior::OpenFolder,
            show_notification: true, 
            show_notification_window: false, 
            notification_message: String::new(), 
            last_indexing_start: Instant::now(), 
            custom_path: None, 
            indexing_file_count: Arc::new(Mutex::new(0)), 
            total_files_to_index: Arc::new(Mutex::new(0)),
            indexing_completed_time: None, // Initialize to None
            search_mode: SearchMode::CompleteSearch, // Default to complete search
            skipped_paths: Self::load_skipped_paths(),
            new_skipped_path: String::new(), 
            show_skip_paths_window: false,
            skip_paths_current_page: 0, 
            skip_paths_per_page: 10,
            custom_file_types: Vec::new(),
            new_file_type: String::new(), 
            show_file_types_window: false,
            show_about_window: false, // Initialize to false
        }
    }
}

impl SurchApp {
    fn start_indexing(&mut self) {
        if self.is_indexing {
            return;
        }

        self.is_indexing = true;
        self.indexing_progress = 0.0;
        self.last_indexing_start = Instant::now(); // Record start time
        
        // Reset indexing progress counters
        *self.indexing_file_count.lock().unwrap() = 0;
        *self.total_files_to_index.lock().unwrap() = 0;
        
        let index = self.index.clone();
        let selected_drives = self.selected_drives.clone();
        let custom_path = self.custom_path.clone();
        let indexing_file_count = self.indexing_file_count.clone();
        let _total_files_to_index = self.total_files_to_index.clone(); 
        let skipped_paths = self.skipped_paths.clone();

        std::thread::spawn(move || {
            println!("Starting indexing...");

            {
                let mut index = index.lock().unwrap();
                index.files.clear();
                index.folders.clear();
                index.file_type_map.clear();
            }

            if let Some(path) = custom_path {
                println!("Indexing custom path: {:?}", path);
                let index_copy = index.clone();
                
                let walker = WalkBuilder::new(&path)
                    .hidden(false)
                    .follow_links(false)
                    .add_custom_ignore_filename(".gitignore")
                    .filter_entry(move |entry| {
                        let path = entry.path();
                        if let Some(file_name) = path.file_name() {
                            if let Some(name) = file_name.to_str() {
                                // Skip folders in the skipped paths list
                                let name_lower = name.to_lowercase();
                                for skipped_path in &skipped_paths {
                                    if name_lower == skipped_path.to_lowercase() {
                                        return false;
                                    }
                                }
                            }
                        }
                        true
                    })
                    .threads(num_cpus::get() * 8)
                    .build_parallel();

                use std::sync::atomic::{AtomicUsize, Ordering};
                let file_count = AtomicUsize::new(0);
                let folder_count = AtomicUsize::new(0);

                walker.run(|| {
                    let thread_index = index_copy.clone();
                    let file_count = &file_count;
                    let folder_count = &folder_count;
                    let indexing_file_count = indexing_file_count.clone();
                    Box::new(move |result| {
                        match result {
                            Ok(entry) => {
                                let path = entry.path().to_path_buf();
                                if path.exists() {
                                    if let Ok(mut index) = thread_index.try_lock() {
                                        if let Some(file_type) = entry.file_type() {
                                            if file_type.is_dir() {
                                                index.add_folder(path);
                                                folder_count.fetch_add(1, Ordering::Relaxed);
                                            } else {
                                                index.add_file(path);
                                                file_count.fetch_add(1, Ordering::Relaxed);
                                            }
                                            *indexing_file_count.lock().unwrap() += 1;
                                        }
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                        ignore::WalkState::Continue
                    })
                });

                println!("Custom path indexed: {} files, {} folders", file_count.load(Ordering::Relaxed), folder_count.load(Ordering::Relaxed));
            } else {
                // Get selected drives
                let mut drives = Vec::new();
                for drive in &selected_drives {
                    let drive_path = format!(r"{}:\", drive);
                    let path = Path::new(&drive_path);
                    if path.exists() {
                        drives.push(path.to_path_buf());
                        println!("Adding drive for indexing: {:?}", drive_path);
                    }
                }

                let mut handles = Vec::new();
                for drive in drives {
                    let index_copy = index.clone();
                    let indexing_file_count = indexing_file_count.clone();
                    let skipped_paths = skipped_paths.clone();
                    
                    let handle = std::thread::spawn(move || {
                        println!("Indexing drive: {:?}", drive);
                        
                        let walker = WalkBuilder::new(&drive)
                            .hidden(false)
                            .follow_links(false)
                            .add_custom_ignore_filename(".gitignore")
                            .filter_entry(move |entry| {
                                let path = entry.path();
                                if let Some(file_name) = path.file_name() {
                                    if let Some(name) = file_name.to_str() {
                                        // Skip folders in the skipped paths list
                                        let name_lower = name.to_lowercase();
                                        for skipped_path in &skipped_paths {
                                            if name_lower == skipped_path.to_lowercase() {
                                                return false;
                                            }
                                        }
                                    }
                                }
                                true
                            })
                            .threads(num_cpus::get() * 8) // Increase thread count for maximum CPU utilization
                            .build_parallel();

                        use std::sync::atomic::{AtomicUsize, Ordering};
                        let file_count = AtomicUsize::new(0);
                        let folder_count = AtomicUsize::new(0);

                        walker.run(|| {
                            let thread_index = index_copy.clone();
                            let file_count = &file_count;
                            let folder_count = &folder_count;
                            let indexing_file_count = indexing_file_count.clone();
                            Box::new(move |result| {
                                match result {
                                    Ok(entry) => {
                                        let path = entry.path().to_path_buf();
                                        if path.exists() {
                                            if let Ok(mut index) = thread_index.try_lock() {
                                                if let Some(file_type) = entry.file_type() {
                                                    if file_type.is_dir() {
                                                        index.add_folder(path);
                                                        folder_count.fetch_add(1, Ordering::Relaxed);
                                                    } else {
                                                        index.add_file(path);
                                                        file_count.fetch_add(1, Ordering::Relaxed);
                                                    }
                                                    // Update indexing progress
                                                    *indexing_file_count.lock().unwrap() += 1;
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {}
                                }
                                ignore::WalkState::Continue
                            })
                        });

                        let final_file_count = file_count.load(Ordering::Relaxed);
                        let final_folder_count = folder_count.load(Ordering::Relaxed);
                        println!("Drive {:?} indexed: {} files, {} folders", drive, final_file_count, final_folder_count);
                    });
                    
                    handles.push(handle);
                }
                
                // Wait for all threads to complete
                for handle in handles {
                    if let Err(e) = handle.join() {
                        println!("Error joining thread: {:?}", e);
                    }
                }
            }

            println!("Indexing completed!");

        });
    }

    // Force search after indexing completes
    fn force_search(&mut self) {
        self.perform_search();
    }

    // Load skipped paths from JSON file
    fn load_skipped_paths() -> Vec<String> {
        let path = Path::new("skipped_paths.json");
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(skipped_paths) = serde_json::from_str::<Vec<String>>(&content) {
                    return skipped_paths;
                }
            }
        }
        // Default skipped paths
        vec![
            "temp".to_string(),
            "tmp".to_string(),
            "system32".to_string(),
            "windows".to_string(),
            "program files".to_string(),
            "program files (x86)".to_string(),
        ]
    }

    // Save skipped paths to JSON file
    fn save_skipped_paths(&self) {
        let path = Path::new("skipped_paths.json");
        if let Ok(content) = serde_json::to_string(&self.skipped_paths) {
            let _ = std::fs::write(path, content);
        }
    }

    fn perform_search(&mut self) {

        let query = self.search_query.clone();
        
        self.search_results.clear();
        
        // Check cache first
        if self.cache_results {
            if let Some((_, cached_results)) = self.search_cache.iter().find(|(cached_query, _)| *cached_query == query) {
                self.search_results = cached_results.clone();
                self.current_page = 0;
                return;
            }
        }

        if let Ok(index) = self.index.try_lock() {
            let results = index.search(&query, 0.5); 
            
            self.search_results = results.clone();
            self.current_page = 0;

            if self.cache_results {
                if self.search_cache.len() >= self.cache_size {
                    self.search_cache.pop_front();
                }
                self.search_cache.push_back((query, results));
            }
        }
    }
}

impl eframe::App for SurchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Surch - Fast File Search");
            if self.is_indexing {
                if Instant::now() - self.last_indexing_start > Duration::from_secs(300) {
                    self.is_indexing = false;
                    self.indexing_progress = 1.0;
                    self.force_search();
                } else {
                    let indexed = *self.indexing_file_count.lock().unwrap();
                    let scale_factor = 10000.0;
                    let progress = if indexed > 0 {
                        (indexed as f32 / scale_factor).min(0.99)
                    } else {
                        0.0
                    };
                    self.indexing_progress = progress; 
                    
                    // Check if indexing is complete
                    if self.indexing_progress >= 0.99 {
                        if self.indexing_completed_time.is_none() {
                            self.indexing_completed_time = Some(Instant::now());
                        } else if Instant::now() - self.indexing_completed_time.unwrap() > Duration::from_secs(3) {
                            self.is_indexing = false;
                            self.indexing_completed_time = None;
                            self.force_search();
                        }
                    } else {
                        self.indexing_completed_time = None;
                    }
                }
            }

            // Search input
            ui.horizontal(|ui| {
                ui.label("Search:");
                let search_response = ui.text_edit_singleline(&mut self.search_query);

                if search_response.changed() {
                    self.last_search_time = Instant::now();
                }

                if Instant::now() - self.last_search_time > self.search_debounce {
                    if self.search_mode == SearchMode::ProgressiveSearch || !self.is_indexing {
                        self.perform_search();
                    }
                }
            });

            // Custom path selection
            ui.horizontal(|ui| {
                ui.label("Path:");
                if let Some(path) = &self.custom_path {
                    ui.label(path.to_string_lossy());
                } else {
                    ui.label("All drives");
                }
                if ui.button("Select Path").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.custom_path = Some(path);
                        // Re-index when path changes
                        self.start_indexing();
                    }
                }
                if ui.button("Clear Path").clicked() {
                    self.custom_path = None;
                    // Re-index when path is cleared
                    self.start_indexing();
                }
            });

            if self.search_query.is_empty() {
                ui.label(egui::RichText::new("Please enter a file name").color(egui::Color32::RED));
            }

            // Drive selection
            ui.horizontal(|ui| {
                ui.label("Drives:");
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    for drive in &self.available_drives {
                        let mut is_selected = self.selected_drives.contains(drive);
                        if ui.checkbox(&mut is_selected, format!("{}:", drive)).changed() {
                            if is_selected && !self.selected_drives.contains(drive) {
                                self.selected_drives.push(drive.clone());
                            } else if !is_selected {
                                self.selected_drives.retain(|d| d != drive);
                            }
                        }
                    }
                });
            });

            // Filters and buttons
            ui.horizontal(|ui| {
                // Category filter
                ui.label("Category:");
                egui::ComboBox::from_id_source("category")
                    .selected_text(
                        self.selected_category
                            .as_deref()
                            .unwrap_or("All")
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.selected_category, 
                            None, 
                            "All"
                        );
                        ui.selectable_value(
                            &mut self.selected_category, 
                            Some("Files".to_string()), 
                            "Files"
                        );
                        ui.selectable_value(
                            &mut self.selected_category, 
                            Some("Folders".to_string()), 
                            "Folders"
                        );
                    });

                // File type filter
                ui.label("File Type:");
                egui::ComboBox::from_id_source("file_type")
                    .selected_text(
                        self.selected_file_type
                            .as_deref()
                            .unwrap_or("All")
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.selected_file_type, 
                            None, 
                            "All"
                        );
                        ui.selectable_value(
                            &mut self.selected_file_type, 
                            Some("txt".to_string()), 
                            "Text"
                        );
                        ui.selectable_value(
                            &mut self.selected_file_type, 
                            Some("jpg".to_string()), 
                            "Image"
                        );
                        ui.selectable_value(
                            &mut self.selected_file_type, 
                            Some("pdf".to_string()), 
                            "PDF"
                        );
                        ui.selectable_value(
                            &mut self.selected_file_type, 
                            Some("docx".to_string()), 
                            "Word"
                        );
                        
                        // Add custom file types
                        for file_type in &self.custom_file_types {
                            ui.selectable_value(
                                &mut self.selected_file_type, 
                                Some(file_type.clone()), 
                                file_type
                            );
                        }
                        
                        // Add More option
                        if ui.button("Add More...").clicked() {
                            self.show_file_types_window = true;
                        }
                    });
            });

            // Start indexing button and settings button
            ui.horizontal(|ui| {
                let button_text = if self.is_indexing {
                    "Indexing..."
                } else {
                    "Start Indexing"
                };
                if ui.button(button_text).clicked() {
                    self.start_indexing();
                }

                if ui.button("Settings").clicked() {
                    self.show_settings = true;
                }

                if ui.button("About").clicked() {
                    self.show_about_window = true;
                }
            });

            // Progress bar
            if self.is_indexing {
                let progress_text = if self.indexing_progress >= 0.99 {
                    "Complete"
                } else {
                    "Indexing..."
                };
                let progress_color = if self.indexing_progress >= 0.99 {
                    egui::Color32::GREEN
                } else {
                    ui.style().visuals.widgets.active.bg_fill
                };
                
                let mut progress_bar = egui::ProgressBar::new(self.indexing_progress)
                    .fill(progress_color);
                
                if self.indexing_progress >= 0.99 {
                    let styled_text = egui::RichText::new(progress_text).color(egui::Color32::WHITE);
                    progress_bar = progress_bar.text(styled_text);
                } else {
                    progress_bar = progress_bar.text(progress_text);
                }
                
                ui.add(progress_bar);
            }

            // Settings window
            if self.show_settings {
                egui::Window::new("Settings")
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.heading("Search Settings");

                        // Search speed slider
                        ui.add(egui::Slider::new(&mut self.search_speed, 1..=10)
                            .text("Search Speed"));
                        ui.label("Higher values use more CPU for faster searches");

                        // Search options
                        ui.checkbox(&mut self.use_parallel_search, "Use Parallel Search");
                        ui.checkbox(&mut self.use_trie_search, "Use Trie Search");
                        ui.checkbox(&mut self.cache_results, "Cache Results");

                        // Cache size
                        if self.cache_results {
                            ui.add(egui::Slider::new(&mut self.cache_size, 10..=1000)
                                .text("Cache Size"));
                        }

                        // Click behavior
                        ui.separator();
                        ui.heading("Click Behavior");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.click_behavior, ClickBehavior::CopyPath, "Copy path to clipboard");
                            ui.radio_value(&mut self.click_behavior, ClickBehavior::OpenFolder, "Open containing folder");
                        });

                        // Notification settings
                        ui.checkbox(&mut self.show_notification, "Show notifications");

                        // Search mode
                        ui.separator();
                        ui.heading("Search Mode");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.search_mode, SearchMode::CompleteSearch, "Complete Search");
                            ui.radio_value(&mut self.search_mode, SearchMode::ProgressiveSearch, "Progressive Search");
                        });
                        ui.label("Complete Search: Shows results after indexing is complete");
                        ui.label("Progressive Search: Shows results as indexing progresses");

                        // Skip paths configuration
                        ui.separator();
                        ui.heading("Skip Paths");
                        ui.label("Configure folders to skip during indexing");
                        if ui.button("Configure Skip Paths").clicked() {
                            self.show_skip_paths_window = true;
                        }

                        if ui.button("Close").clicked() {
                            self.show_settings = false;
                        }
                    });
            }

            // Notification window
            if self.show_notification_window {
                egui::Window::new("Notification")
                    .auto_sized()
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label(egui::RichText::new(&self.notification_message).color(egui::Color32::GREEN));
                        if ui.button("OK").clicked() {
                            self.show_notification_window = false;
                        }
                    });
            }

            if self.show_skip_paths_window {
                egui::Window::new("Configure Skip Paths")
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.heading("Skip Paths Configuration");
                        ui.label("Add or remove folder names to skip during indexing");
                        ui.separator();
                        ui.heading("Current Skip Paths");
                        
                        // Pagination for skip paths
                        let total_skip_paths_pages = (self.skipped_paths.len() + self.skip_paths_per_page - 1) / self.skip_paths_per_page;
                        let skip_paths_start_idx = self.skip_paths_current_page * self.skip_paths_per_page;
                        let skip_paths_end_idx = (self.skip_paths_current_page + 1) * self.skip_paths_per_page;
                        let current_page_paths = &self.skipped_paths[skip_paths_start_idx..skip_paths_end_idx.min(self.skipped_paths.len())];
                        
                        let mut paths_to_remove = Vec::new();
                        for (i, path) in current_page_paths.iter().enumerate() {
                            let actual_index = skip_paths_start_idx + i;
                            ui.horizontal(|ui| {
                                ui.label(path);
                                if ui.button("Remove").clicked() {
                                    paths_to_remove.push(actual_index);
                                }
                            });
                        }
                        for i in paths_to_remove.iter().rev() {
                            self.skipped_paths.remove(*i);
                            if self.skip_paths_current_page > 0 && skip_paths_start_idx >= self.skipped_paths.len() {
                                self.skip_paths_current_page -= 1;
                            }
                        }
                        
                        ui.horizontal(|ui| {
                            if self.skip_paths_current_page > 0 {
                                if ui.button("Previous").clicked() {
                                    self.skip_paths_current_page -= 1;
                                }
                            }
                            
                            ui.label(format!("{}/{} pages", self.skip_paths_current_page + 1, total_skip_paths_pages.max(1)));
                            
                            if total_skip_paths_pages > 1 && self.skip_paths_current_page < total_skip_paths_pages - 1 {
                                if ui.button("Next").clicked() {
                                    self.skip_paths_current_page += 1;
                                }
                            }
                        });

                        ui.separator();
                        ui.heading("Add New Skip Path");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.new_skipped_path).hint_text("Enter folder name to skip"));
                            if ui.button("Add").clicked() && !self.new_skipped_path.is_empty() {
                                if !self.skipped_paths.contains(&self.new_skipped_path) {
                                    self.skipped_paths.push(self.new_skipped_path.clone());
                                    self.new_skipped_path.clear();
                                }
                            }
                        });

                        // Save and close
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                self.save_skipped_paths();
                                self.show_skip_paths_window = false;
                            }
                            if ui.button("Cancel").clicked() {
                                self.show_skip_paths_window = false;
                            }
                        });
                    });
            }

            // File types configuration window
            if self.show_file_types_window {
                egui::Window::new("Configure File Types")
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.heading("File Types Configuration");
                        ui.label("Add or remove custom file types");

                        // Current custom file types
                        ui.separator();
                        ui.heading("Current Custom File Types");
                        let mut types_to_remove = Vec::new();
                        for (i, file_type) in self.custom_file_types.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(file_type);
                                if ui.button("Remove").clicked() {
                                    types_to_remove.push(i);
                                }
                            });
                        }
                        // Remove types in reverse order to avoid index shifting
                        for i in types_to_remove.iter().rev() {
                            self.custom_file_types.remove(*i);
                        }

                        // Add new file type
                        ui.separator();
                        ui.heading("Add New File Type");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.new_file_type).hint_text("Enter file extension (e.g., 'jpg')"));
                            if ui.button("Add").clicked() && !self.new_file_type.is_empty() {
                                let file_type = self.new_file_type.trim().to_lowercase();
                                if !self.custom_file_types.contains(&file_type) {
                                    self.custom_file_types.push(file_type);
                                    self.new_file_type.clear();
                                }
                            }
                        });

                        // Close button
                        ui.separator();
                        if ui.button("Close").clicked() {
                            self.show_file_types_window = false;
                        }
                    });
            }

            // About window
            if self.show_about_window {
                egui::Window::new("About Surch")
                    .resizable(true)
                    .show(ctx, |ui| {
                        ui.heading("Surch - Fast File Search");
                        ui.separator();
                        ui.label("A fast and efficient file search application");
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Creator:");
                            ui.label(egui::RichText::new("Eacon").strong());
                        });
                        ui.horizontal(|ui| {
                            ui.label("Contact:");
                            ui.hyperlink("17891931241@163.com");
                        });
                        ui.separator();
                        ui.label("If you encounter any issues, please feel free to contact the creator.");
                        ui.separator();
                        if ui.button("Close").clicked() {
                            self.show_about_window = false;
                        }
                    });
            }

            // Search results
            if !self.search_query.is_empty() {
                ui.separator();
                ui.heading("Results:");

                // Filter results by category and file type
                let filtered_results: Vec<_> = self.search_results
                    .iter()
                    .take(self.max_results)
                    .filter(|(path, _, is_folder)| {
                        // Filter by category
                        let category_match = match &self.selected_category {
                            None => true,
                            Some(category) => {
                                if category == "Files" {
                                    !is_folder
                                } else if category == "Folders" {
                                    *is_folder
                                } else {
                                    true
                                }
                            }
                        };

                        // Filter by file type
                        let file_type_match = match &self.selected_file_type {
                            None => true,
                            Some(file_type) => {
                                if *is_folder {
                                    true
                                } else {
                                    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
                                        extension.to_lowercase() == *file_type
                                    } else {
                                        false
                                    }
                                }
                            }
                        };

                        category_match && file_type_match
                    })
                    .collect();

                // Pagination
                let total_pages = (filtered_results.len() + self.results_per_page - 1) / self.results_per_page;
                // Ensure current page is within bounds
                if self.current_page >= total_pages && total_pages > 0 {
                    self.current_page = total_pages - 1;
                }
                let start_idx = self.current_page * self.results_per_page;
                let end_idx = (self.current_page + 1) * self.results_per_page;
                let page_results = &filtered_results[start_idx..end_idx.min(filtered_results.len())];

                // Display results
                if page_results.is_empty() {
                    ui.label(egui::RichText::new("No results found").color(egui::Color32::RED));
                } else {
                    for (path, score, is_folder) in page_results {
                        let path_str = path.to_string_lossy();
                        let name = path.file_name().unwrap_or_default().to_string_lossy();

                        let mut label = egui::RichText::new(format!("{} - {:.1}% match", name, score * 100.0));
                        if *is_folder {
                            label = label.strong();
                        }

                        // Make result clickable
                        if ui.add(egui::Button::new(label)).clicked() {
                            match self.click_behavior {
                                ClickBehavior::CopyPath => {
                                    if let Ok(mut clipboard) = ClipboardContext::new() {
                                        if clipboard.set_contents(path_str.to_string()).is_ok() {
                                            if self.show_notification {
                                                self.notification_message = "Path copied to clipboard".to_string();
                                                self.show_notification_window = true;
                                                ctx.request_repaint();
                                            }
                                        }
                                    }
                                }
                                ClickBehavior::OpenFolder => {
                                    // Open folder directly if it's a folder, otherwise open containing folder
                                    if *is_folder {
                                        // It's a folder, open it directly
                                        let _ = that(&path);
                                    } else {
                                        // It's a file, open containing folder
                                        if let Some(parent) = path.parent() {
                                            let _ = that(parent);
                                        }
                                    }
                                }
                            }
                        }
                        ui.label(egui::RichText::new(path_str).small().weak());
                        ui.separator();
                    }
                }

                // Pagination controls
                ui.horizontal(|ui| {
                    if self.current_page > 0 {
                        if ui.button("Previous").clicked() {
                            self.current_page -= 1;
                            ctx.request_repaint();
                        }
                    }

                    ui.label(format!("{}/{} pages", self.current_page + 1, total_pages.max(1)));

                    if total_pages > 1 && self.current_page < total_pages - 1 {
                        if ui.button("Next").clicked() {
                            self.current_page += 1;
                            ctx.request_repaint();
                        }
                    }
                });

                // Search more button
                if filtered_results.len() >= self.max_results {
                    if ui.button("Search More").clicked() {
                        self.max_results += 100;
                    }
                }
            }
        });
    }
}

fn main() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Surch",
        options,
        Box::new(|_| Box::new(SurchApp::default())),
    )
    .unwrap();
}
