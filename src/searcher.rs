use std::collections::HashMap;
use std::fs;
use std::fs::DirEntry;
use std::fs::File;
use std::fs::Metadata;
use std::fs::symlink_metadata;
use std::path::Path;
use std::path::PathBuf;
use std::io;
use std::io::BufReader;
use std::io::Read;
use std::rc::Rc;

use chrono::{Datelike, DateTime, Local};
use csv;
use humansize::{FileSize, file_size_opts};
use imagesize;
use mp3_metadata;
use mp3_metadata::MP3Metadata;
use serde_json;
use term::StdoutTerminal;
#[cfg(unix)]
use users::{Groups, Users, UsersCache};
#[cfg(unix)]
use xattr::FileExt;
use zip;

use field::Field;
use fileinfo::FileInfo;
use fileinfo::to_file_info;
use function::Function;
use gitignore::GitignoreFilter;
use gitignore::matches_gitignore_filter;
use gitignore::parse_gitignore;
use mode;
use parser::ColumnExpr;
use parser::Query;
use parser::Expr;
use parser::LogicalOp;
use parser::Op;
use parser::OutputFormat;
use util::*;

pub struct Searcher {
    query: Query,
    user_cache: UsersCache,
    found: u32,
    raw_output_buffer: Vec<HashMap<String, String>>,
    output_buffer: TopN<Criteria<String>, String>,
    gitignore_map: HashMap<PathBuf, Vec<GitignoreFilter>>,
}

impl Searcher {
    pub fn new(query: Query) -> Self {
        let limit = query.limit;
        Searcher {
            query,
            user_cache: UsersCache::new(),
            found: 0,
            raw_output_buffer: vec![],
            output_buffer: if limit == 0 { TopN::limitless() } else { TopN::new(limit) },
            gitignore_map: HashMap::new(),
        }
    }

    pub fn is_buffered(&self) -> bool {
        self.has_ordering() || self.has_aggregate_column()
    }

    fn has_ordering(&self) -> bool {
        !self.query.ordering_fields.is_empty()
    }

    fn has_aggregate_column(&self) -> bool {
        self.query.fields.iter().any(|ref f| f.has_aggregate_function())
    }

    fn print_results_start(&self) {
        if let OutputFormat::Json = self.query.output_format {
            print!("[");
        }
    }

    fn format_results_row(&self, record: String,
                          mut output_value: String,
                          records: &mut Vec<String>) -> String {
        match self.query.output_format {
            OutputFormat::Lines => {
                output_value.push_str(&record);
                output_value.push('\n');
            },
            OutputFormat::List => {
                output_value.push_str(&record);
                output_value.push('\0');
            },
            OutputFormat::Json => {
                // use file_map later
            },
            OutputFormat::Tabs => {
                output_value.push_str(&record);
                output_value.push('\t');
            },
            OutputFormat::Csv => {
                records.push(record);
            },
        }

        output_value
    }

    fn format_results_row_end(&self,
                              mut output_value: String,
                              records: &Vec<String>,
                              file_map: &HashMap<String, String>) -> String {
        match self.query.output_format {
            OutputFormat::Lines | OutputFormat::List => {},
            OutputFormat::Tabs => {
                output_value.push('\n');
            },
            OutputFormat::Csv => {
                let mut csv_output = WritableBuffer::new();
                {
                    let mut csv_writer = csv::Writer::from_writer(&mut csv_output);
                    let _ = csv_writer.write_record(records);
                }
                let result: String = csv_output.into();
                output_value.push_str(result.as_ref());
            },
            OutputFormat::Json => {
                if !self.is_buffered() && self.found > 1 {
                    output_value.push(',');
                }
                output_value.push_str(&serde_json::to_string(&file_map).unwrap());
            },
        }

        output_value
    }

    fn print_results_end(&self) {
        if let OutputFormat::Json = self.query.output_format {
            print!("]");
        }
    }

    pub fn list_search_results(&mut self, t: &mut Box<StdoutTerminal>) -> io::Result<()> {
        let need_metadata = self.query.get_all_fields().iter().any(|f| f != &Field::Name);
        let need_dim = self.query.get_all_fields().iter().any(|f| f == &Field::Width || f == &Field::Height);
        let need_mp3 = self.query.get_all_fields().iter().any(|f| f.is_mp3_field());

        self.print_results_start();

        for root in &self.query.clone().roots {
            let root_dir = Path::new(&root.path);
            let min_depth = root.min_depth;
            let max_depth = root.max_depth;
            let search_archives = root.archives;
            let follow_symlinks = root.symlinks;
            let apply_gitignore = root.gitignore;
            let _result = self.visit_dirs(
                root_dir,
                need_metadata,
                need_dim,
                need_mp3,
                min_depth,
                max_depth,
                1,
                search_archives,
                follow_symlinks,
                apply_gitignore,
                t
            );
        }

        if self.has_aggregate_column() {
            let mut records = vec![];
            let mut file_map = HashMap::new();
            let mut output_value = String::new();

            for column_expr in &self.query.fields {
                let record = format!("{}", self.get_aggregate_function_value(column_expr));
                file_map.insert(column_expr.to_string().to_lowercase(), record.clone());

                output_value = self.format_results_row(record, output_value, &mut records);
            }

            output_value = self.format_results_row_end(output_value, &records, &file_map);

            print!("{}", output_value);
        } else if self.is_buffered() {
            let mut first = true;
            for piece in self.output_buffer.values() {
                if let OutputFormat::Json = self.query.output_format {
                    if first {
                        first = false;
                    } else {
                        print!(",");
                    }
                }
                print!("{}", piece);
            }
        }

        self.print_results_end();

        Ok(())
    }

    fn visit_dirs(&mut self,
                  dir: &Path,
                  need_metadata: bool,
                  need_dim: bool,
                  need_mp3: bool,
                  min_depth: u32,
                  max_depth: u32,
                  depth: u32,
                  search_archives: bool,
                  follow_symlinks: bool,
                  apply_gitignore: bool,
                  t: &mut Box<StdoutTerminal>) -> io::Result<()> {
        if (min_depth == 0 || (min_depth > 0 && depth >= min_depth)) && (max_depth == 0 || (max_depth > 0 && depth <= max_depth)) {
            let metadata = match follow_symlinks {
                true => dir.metadata(),
                false => symlink_metadata(dir)
            };
            match metadata {
                Ok(metadata) => {
                    if metadata.is_dir() {
                        let mut gitignore_filters = None;

                        if apply_gitignore {
                            let gitignore_file = dir.join(".gitignore");
                            if gitignore_file.is_file() {
                                let regexes = parse_gitignore(&gitignore_file, dir);
                                self.gitignore_map.insert(dir.to_path_buf(), regexes);
                            }

                            gitignore_filters = Some(self.get_gitignore_filters(dir));
                        }

                        match fs::read_dir(dir) {
                            Ok(entry_list) => {
                                for entry in entry_list {
                                    if !self.is_buffered() && self.query.limit > 0 && self.query.limit <= self.found {
                                        break;
                                    }

                                    match entry {
                                        Ok(entry) => {
                                            let path = entry.path();

                                            if !apply_gitignore || (apply_gitignore && !matches_gitignore_filter(&gitignore_filters, entry.path().to_string_lossy().as_ref(), path.is_dir())) {
                                                self.check_file(&entry, &None, need_metadata, need_dim, need_mp3, follow_symlinks, t);

                                                if search_archives && is_zip_archive(&path.to_string_lossy()) {
                                                    if let Ok(file) = fs::File::open(&path) {
                                                        if let Ok(mut archive) = zip::ZipArchive::new(file) {
                                                            for i in 0..archive.len() {
                                                                if self.query.limit > 0 && self.query.limit <= self.found {
                                                                    break;
                                                                }

                                                                if let Ok(afile) = archive.by_index(i) {
                                                                    let file_info = to_file_info(&afile);
                                                                    self.check_file(&entry, &Some(file_info), need_metadata, need_dim, need_mp3, false, t);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }

                                                if path.is_dir() {
                                                    let result = self.visit_dirs(
                                                        &path,
                                                        need_metadata,
                                                        need_dim,
                                                        need_mp3,
                                                        min_depth,
                                                        max_depth,
                                                        depth + 1,
                                                        search_archives,
                                                        follow_symlinks,
                                                        apply_gitignore,
                                                        t);

                                                    if result.is_err() {
                                                        path_error_message(&path, result.err().unwrap(), t);
                                                    }
                                                }
                                            }
                                        },
                                        Err(err) => {
                                            path_error_message(dir, err, t);
                                        }
                                    }
                                }
                            },
                            Err(err) => {
                                path_error_message(dir, err, t);
                            }
                        }
                    }
                },
                Err(err) => {
                    path_error_message(dir, err, t);
                }
            }
        }

        Ok(())
    }

    fn get_gitignore_filters(&self, dir: &Path) -> Vec<GitignoreFilter> {
        let mut result = vec![];

        for (dir_path, regexes) in &self.gitignore_map {
            if dir.to_path_buf() == *dir_path {
                for ref mut rx in regexes {
                    result.push(rx.clone());
                }

                return result;
            }
        }

        let mut path = dir.to_path_buf();

        loop {
            let parent_found = path.pop();

            if !parent_found {
                return result;
            }

            for (dir_path, regexes) in &self.gitignore_map {
                if path == *dir_path {
                    let mut tmp = vec![];
                    for ref mut rx in regexes {
                        tmp.push(rx.clone());
                    }
                    tmp.append(&mut result);
                    result.clear();
                    result.append(&mut tmp);
                }
            }
        }
    }

    fn get_column_expr_value(&self,
                             entry: &DirEntry,
                             file_info: &Option<FileInfo>,
                             mp3_info: &Option<MP3Metadata>,
                             attrs: &Option<Box<Metadata>>,
                             dimensions: Option<(usize, usize)>,
                             column_expr: &ColumnExpr,
                             _t: &mut Box<StdoutTerminal>) -> String {
        if let Some(ref _function) = column_expr.function {
            return self.get_function_value(entry, file_info, mp3_info, attrs, dimensions, column_expr, _t);
        }

        if let Some(ref field) = column_expr.field {
            return self.get_field_value(entry, file_info, mp3_info, attrs, dimensions, field, _t);
        }

        if let Some(ref value) = column_expr.val {
            return value.clone();
        }

        String::new()
    }

    fn get_function_value(&self,
                          entry: &DirEntry,
                          file_info: &Option<FileInfo>,
                          mp3_info: &Option<MP3Metadata>,
                          attrs: &Option<Box<Metadata>>,
                          dimensions: Option<(usize, usize)>,
                          column_expr: &ColumnExpr,
                          _t: &mut Box<StdoutTerminal>) -> String {
        if let Some(ref left_expr) = column_expr.left {
            let function_arg = self.get_column_expr_value(entry,
                                                          file_info,
                                                          mp3_info,
                                                          attrs,
                                                          dimensions,
                                                          left_expr,
                                                          _t);

            match column_expr.function {
                Some(Function::Lower) => {
                    return function_arg.to_lowercase();
                },
                Some(Function::Upper) => {
                    return function_arg.to_uppercase();
                },
                Some(Function::Length) => {
                    return format!("{}", function_arg.chars().count());
                },
                Some(Function::Year) => {
                    match parse_datetime(&function_arg) {
                        Ok(date) => {
                            return date.0.year().to_string();
                        },
                        _ => {
                            return String::new();
                        }
                    }
                },
                Some(Function::Month) => {
                    match parse_datetime(&function_arg) {
                        Ok(date) => {
                            return date.0.month().to_string();
                        },
                        _ => {
                            return String::new();
                        }
                    }
                },
                Some(Function::Day) => {
                    match parse_datetime(&function_arg) {
                        Ok(date) => {
                            return date.0.day().to_string();
                        },
                        _ => {
                            return String::new();
                        }
                    }
                },
                _ => {
                    return String::new();
                }
            }
        }

        String::new()
    }

    fn get_aggregate_function_value(&self,
                                    column_expr: &ColumnExpr) -> String {
        let mut field_value = String::new();

        if let Some(ref field) = column_expr.field {
            field_value = field.to_string();
        } else if let Some(ref left) = column_expr.left  {
            if let Some(ref field) = left.field {
                field_value = field.to_string();
            }
        }

        let field = field_value.to_lowercase();
        match column_expr.function {
            Some(Function::Min) => {
                let mut min = -1;
                for value in &self.raw_output_buffer {
                    if let Some(value) = value.get(&field) {
                        if let Ok(value) = value.parse::<i64>() {
                            if value < min || min == -1 {
                                min = value;
                            }
                        }
                    }
                }

                return min.to_string();
            },
            Some(Function::Max) => {
                let mut max = 0;
                for value in &self.raw_output_buffer {
                    if let Some(value) = value.get(&field) {
                        if let Ok(value) = value.parse::<usize>() {
                            if value > max {
                                max = value;
                            }
                        }
                    }
                }

                return max.to_string();
            },
            Some(Function::Avg) => {
                let mut sum = 0;
                for value in &self.raw_output_buffer {
                    if let Some(value) = value.get(&field) {
                        if let Ok(value) = value.parse::<usize>() {
                            sum += value;
                        }
                    }
                }

                return (sum / self.raw_output_buffer.len()).to_string();
            },
            Some(Function::Sum) => {
                let mut sum = 0;
                for value in &self.raw_output_buffer {
                    if let Some(value) = value.get(&field) {
                        if let Ok(value) = value.parse::<usize>() {
                            sum += value;
                        }
                    }
                }

                return sum.to_string();
            },
            Some(Function::Count) => {
                return self.raw_output_buffer.len().to_string();
            },
            _ => {
                match &column_expr.val {
                    Some(val) => return val.clone(),
                    _ => return String::new()
                }
            }
        }
    }

    fn get_field_value(&self,
                       entry: &DirEntry,
                       file_info: &Option<FileInfo>,
                       mp3_info: &Option<MP3Metadata>,
                       attrs: &Option<Box<Metadata>>,
                       dimensions: Option<(usize, usize)>,
                       field: &Field,
                       _t: &mut Box<StdoutTerminal>) -> String {
        match field {
            Field::Name => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("[{}] {}", entry.file_name().to_string_lossy(), file_info.name);
                    },
                    _ => {
                        return format!("{}", entry.file_name().to_string_lossy());
                    }
                }
            },
            Field::Path => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("[{}] {}", entry.path().to_string_lossy(), file_info.name);
                    },
                    _ => {
                        return format!("{}", entry.path().to_string_lossy());
                    }
                }
            },
            Field::Size => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("{}", file_info.size);
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", attrs.len());
                        }
                    }
                }
            },
            Field::FormattedSize => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("{}", file_info.size.file_size(file_size_opts::BINARY).unwrap());
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", attrs.len().file_size(file_size_opts::BINARY).unwrap());
                        }
                    }
                }
            },
            Field::IsDir => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("{}", file_info.name.ends_with('/'));
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", attrs.is_dir());
                        }
                    }
                }
            },
            Field::IsFile => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("{}", !file_info.name.ends_with('/'));
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", attrs.is_file());
                        }
                    }
                }
            },
            Field::IsSymlink => {
                match file_info {
                    Some(_) => {
                        return format!("{}", false);
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", attrs.file_type().is_symlink());
                        }
                    }
                }
            },
            Field::IsPipe => {
                return Self::print_file_mode(&attrs, &mode::is_pipe, &file_info, &mode::mode_is_pipe);
            },
            Field::IsCharacterDevice => {
                return Self::print_file_mode(&attrs, &mode::is_char_device, &file_info, &mode::mode_is_char_device);
            },
            Field::IsBlockDevice => {
                return Self::print_file_mode(&attrs, &mode::is_block_device, &file_info, &mode::mode_is_block_device);
            },
            Field::IsSocket => {
                return Self::print_file_mode(&attrs, &mode::is_socket, &file_info, &mode::mode_is_socket);
            },
            Field::Mode => {
                match file_info {
                    Some(ref file_info) => {
                        if let Some(mode) = file_info.mode {
                            return format!("{}", mode::format_mode(mode));
                        }
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            return format!("{}", mode::get_mode(attrs));
                        }
                    }
                }
            },
            Field::UserRead => {
                return Self::print_file_mode(&attrs, &mode::user_read, &file_info, &mode::mode_user_read);
            },
            Field::UserWrite => {
                return Self::print_file_mode(&attrs, &mode::user_write, &file_info, &mode::mode_user_write);
            },
            Field::UserExec => {
                return Self::print_file_mode(&attrs, &mode::user_exec, &file_info, &mode::mode_user_exec);
            },
            Field::GroupRead => {
                return Self::print_file_mode(&attrs, &mode::group_read, &file_info, &mode::mode_group_read);
            },
            Field::GroupWrite => {
                return Self::print_file_mode(&attrs, &mode::group_write, &file_info, &mode::mode_group_write);
            },
            Field::GroupExec => {
                return Self::print_file_mode(&attrs, &mode::group_exec, &file_info, &mode::mode_group_exec);
            },
            Field::OtherRead => {
                return Self::print_file_mode(&attrs, &mode::other_read, &file_info, &mode::mode_other_read);
            },
            Field::OtherWrite => {
                return Self::print_file_mode(&attrs, &mode::other_write, &file_info, &mode::mode_other_write);
            },
            Field::OtherExec => {
                return Self::print_file_mode(&attrs, &mode::other_exec, &file_info, &mode::mode_other_exec);
            },
            Field::IsHidden => {
                match file_info {
                    Some(ref file_info) => {
                        return format!("{}", is_hidden(&file_info.name, &None, true));
                    },
                    _ => {
                        return format!("{}", is_hidden(&entry.file_name().to_string_lossy(), &attrs, false));
                    }
                }
            },
            Field::Uid => {
                if let Some(ref attrs) = attrs {
                    if let Some(uid) = mode::get_uid(attrs) {
                        return format!("{}", uid);
                    }
                }
            },
            Field::Gid => {
                if let Some(ref attrs) = attrs {
                    if let Some(gid) = mode::get_gid(attrs) {
                        return format!("{}", gid);
                    }
                }
            },
            Field::User => {
                if let Some(ref attrs) = attrs {
                    if let Some(uid) = mode::get_uid(attrs) {
                        if let Some(user) = self.user_cache.get_user_by_uid(uid) {
                            return format!("{}", user.name().to_string_lossy());
                        }
                    }
                }
            },
            Field::Group => {
                if let Some(ref attrs) = attrs {
                    if let Some(gid) = mode::get_gid(attrs) {
                        if let Some(group) = self.user_cache.get_group_by_gid(gid) {
                            return format!("{}", group.name().to_string_lossy());
                        }
                    }
                }
            },
            Field::Created => {
                if let Some(ref attrs) = attrs {
                    if let Ok(sdt) = attrs.created() {
                        let dt: DateTime<Local> = DateTime::from(sdt);
                        let format = dt.format("%Y-%m-%d %H:%M:%S");
                        return format!("{}", format);
                    }
                }
            },
            Field::Accessed => {
                if let Some(ref attrs) = attrs {
                    if let Ok(sdt) = attrs.accessed() {
                        let dt: DateTime<Local> = DateTime::from(sdt);
                        let format = dt.format("%Y-%m-%d %H:%M:%S");
                        return format!("{}", format);
                    }
                }
            },
            Field::Modified => {
                match file_info {
                    Some(ref file_info) => {
                        let dt: DateTime<Local> = to_local_datetime(&file_info.modified);
                        let format = dt.format("%Y-%m-%d %H:%M:%S");
                        return format!("{}", format);
                    },
                    _ => {
                        if let Some(ref attrs) = attrs {
                            if let Ok(sdt) = attrs.modified() {
                                let dt: DateTime<Local> = DateTime::from(sdt);
                                let format = dt.format("%Y-%m-%d %H:%M:%S");
                                return format!("{}", format);
                            }
                        }
                    }
                }
            },
            Field::HasXattrs => {
                #[cfg(unix)]
                    {
                        if let Ok(file) = File::open(&entry.path()) {
                            if let Ok(xattrs) = file.list_xattr() {
                                let has_xattrs = xattrs.count() > 0;
                                return format!("{}", has_xattrs);
                            }
                        }
                    }

                #[cfg(not(unix))]
                    {
                        return format!("{}", false);
                    }
            },
            Field::IsShebang => {
                return format!("{}", is_shebang(&entry.path()));
            },
            Field::Width => {
                if let Some(ref dimensions) = dimensions {
                    return format!("{}", dimensions.0);
                }
            },
            Field::Height => {
                if let Some(ref dimensions) = dimensions {
                    return format!("{}", dimensions.1);
                }
            },
            Field::Bitrate => {
                if let Some(ref mp3_info) = mp3_info {
                    return format!("{}", mp3_info.frames[0].bitrate);
                }
            },
            Field::Freq => {
                if let Some(ref mp3_info) = mp3_info {
                    return format!("{}", mp3_info.frames[0].sampling_freq);
                }
            },
            Field::Title => {
                if let Some(ref mp3_info) = mp3_info {
                    if let Some(ref mp3_tag) = mp3_info.tag {
                        return format!("{}", mp3_tag.title);
                    }
                }
            },
            Field::Artist => {
                if let Some(ref mp3_info) = mp3_info {
                    if let Some(ref mp3_tag) = mp3_info.tag {
                        return format!("{}", mp3_tag.artist);
                    }
                }
            },
            Field::Album => {
                if let Some(ref mp3_info) = mp3_info {
                    if let Some(ref mp3_tag) = mp3_info.tag {
                        return format!("{}", mp3_tag.album);
                    }
                }
            },
            Field::Year => {
                if let Some(ref mp3_info) = mp3_info {
                    if let Some(ref mp3_tag) = mp3_info.tag {
                        return format!("{}", mp3_tag.year);
                    }
                }
            },
            Field::Genre => {
                if let Some(ref mp3_info) = mp3_info {
                    if let Some(ref mp3_tag) = mp3_info.tag {
                        return format!("{:?}", mp3_tag.genre);
                    }
                }
            },
            Field::IsArchive => {
                let is_archive = is_archive(&entry.file_name().to_string_lossy());
                return format!("{}", is_archive);
            },
            Field::IsAudio => {
                let is_audio = is_audio(&entry.file_name().to_string_lossy());
                return format!("{}", is_audio);
            },
            Field::IsBook => {
                let is_book = is_book(&entry.file_name().to_string_lossy());
                return format!("{}", is_book);
            },
            Field::IsDoc => {
                let is_doc = is_doc(&entry.file_name().to_string_lossy());
                return format!("{}", is_doc);
            },
            Field::IsImage => {
                let is_image = is_image(&entry.file_name().to_string_lossy());
                return format!("{}", is_image);
            },
            Field::IsSource => {
                let is_source = is_source(&entry.file_name().to_string_lossy());
                return format!("{}", is_source);
            },
            Field::IsVideo => {
                let is_video = is_video(&entry.file_name().to_string_lossy());
                return format!("{}", is_video);
            }
        };

        return String::new();
    }

    fn check_file(&mut self,
                  entry: &DirEntry,
                  file_info: &Option<FileInfo>,
                  need_metadata: bool,
                  need_dim: bool,
                  need_mp3: bool,
                  follow_symlinks: bool,
                  t: &mut Box<StdoutTerminal>) {
        let mut meta = None;
        let mut dim = None;
        let mut mp3 = None;

        if let Some(ref expr) = self.query.expr.clone() {
            let (result, entry_meta, entry_dim, entry_mp3) = self.conforms(entry, file_info, expr, None, None, None, follow_symlinks);
            if !result {
                return
            }

            meta = entry_meta;
            dim = entry_dim;
            mp3 = entry_mp3;
        }

        self.found += 1;

        let attrs = match need_metadata {
            true => update_meta(entry, meta, follow_symlinks),
            false => None
        };

        let dimensions = match need_dim {
            true => update_img_dimensions(&entry, dim),
            false => None
        };

        let mp3_info = match need_mp3 {
            true => update_mp3_meta(&entry, mp3),
            false => None
        };

        let mut records = vec![];
        let mut file_map = HashMap::new();

        let mut output_value = String::new();
        let mut criteria = vec!["".to_string(); self.query.ordering_fields.len()];

        for field in self.query.get_all_fields() {
            file_map.insert(field.to_string().to_lowercase(), self.get_field_value(entry, file_info, &mp3_info, &attrs, dimensions, &field, t));
        }

        for field in self.query.fields.iter() {
            let mut record = self.get_column_expr_value(entry, file_info, &mp3_info, &attrs, dimensions, &field, t);
            file_map.insert(field.to_string().to_lowercase(), record.clone());

            output_value = self.format_results_row(record, output_value, &mut records);
        }

        for (idx, field) in self.query.ordering_fields.iter().enumerate() {
            criteria[idx] = match file_map.get(&field.to_string().to_lowercase()) {
                Some(record) => record.clone(),
                None => self.get_field_value(entry, file_info, &mp3_info, &attrs, dimensions, &field.clone().field.unwrap(), t)
            }
        }

        output_value = self.format_results_row_end(output_value, &records, &file_map);

        if self.is_buffered() {
            self.output_buffer.insert(Criteria::new(Rc::new(self.query.ordering_fields.clone()), criteria, self.query.ordering_asc.clone()), output_value);

            if self.has_aggregate_column() {
                self.raw_output_buffer.push(file_map);
            }
        } else {
            print!("{}", output_value);
        }
    }

    fn print_file_mode(attrs: &Option<Box<Metadata>>,
                       mode_func_boxed: &Fn(&Box<Metadata>) -> bool,
                       file_info: &Option<FileInfo>,
                       mode_func_i32: &Fn(u32) -> bool) -> String {
        match file_info {
            Some(ref file_info) => {
                if let Some(mode) = file_info.mode {
                    return format!("{}", mode_func_i32(mode));
                }
            },
            _ => {
                if let Some(ref attrs) = attrs {
                    return format!("{}", mode_func_boxed(attrs));
                }
            }
        }

        String::new()
    }

    fn conforms(&mut self,
                entry: &DirEntry,
                file_info: &Option<FileInfo>,
                expr: &Box<Expr>,
                entry_meta: Option<Box<fs::Metadata>>,
                entry_dim: Option<(usize, usize)>,
                entry_mp3: Option<MP3Metadata>,
                follow_symlinks: bool) -> (bool, Option<Box<fs::Metadata>>, Option<(usize, usize)>, Option<MP3Metadata>) {
        let mut result = false;
        let mut meta = entry_meta;
        let mut dim = entry_dim;
        let mut mp3 = entry_mp3;

        if let Some(ref logical_op) = expr.logical_op {
            let mut left_result = false;
            let mut right_result = false;

            if let Some(ref left) = expr.left {
                let (left_res, left_meta, left_dim, left_mp3) = self.conforms(entry, file_info, &left, meta, dim, mp3, follow_symlinks);
                left_result = left_res;
                meta = left_meta;
                dim = left_dim;
                mp3 = left_mp3;
            }

            match logical_op {
                LogicalOp::And => {
                    if !left_result {
                        result = false;
                    } else {
                        if let Some(ref right) = expr.right {
                            let (right_res, right_meta, right_dim, right_mp3) = self.conforms(entry, file_info, &right, meta, dim, mp3, follow_symlinks);
                            right_result = right_res;
                            meta = right_meta;
                            dim = right_dim;
                            mp3 = right_mp3;
                        }

                        result = left_result && right_result;
                    }
                },
                LogicalOp::Or => {
                    if left_result {
                        result = true;
                    } else {
                        if let Some(ref right) = expr.right {
                            let (right_res, right_meta, right_dim, right_mp3) = self.conforms(entry, file_info, &right, meta, dim, mp3, follow_symlinks);
                            right_result = right_res;
                            meta = right_meta;
                            dim = right_dim;
                            mp3 = right_mp3;
                        }

                        result = left_result || right_result
                    }
                }
            }
        }

        if let Some(ref field) = expr.field {
            let field = field.field.clone().unwrap();
            match field {
                Field::Name => {
                    if let Some(ref val) = expr.val {
                        let file_name = match file_info {
                            Some(ref file_info) => file_info.name.clone(),
                            _ => entry.file_name().to_string_lossy().to_string()
                        };

                        result = match expr.op {
                            Some(Op::Eq) => {
                                match expr.regex {
                                    Some(ref regex) => regex.is_match(&file_name),
                                    None => val.eq(&file_name)
                                }
                            },
                            Some(Op::Ne) => {
                                match expr.regex {
                                    Some(ref regex) => !regex.is_match(&file_name),
                                    None => val.ne(&file_name)
                                }
                            },
                            Some(Op::Rx) | Some(Op::Like) => {
                                match expr.regex {
                                    Some(ref regex) => regex.is_match(&file_name),
                                    None => false
                                }
                            },
                            Some(Op::Eeq) => {
                                val.eq(&file_name)
                            },
                            Some(Op::Ene) => {
                                val.ne(&file_name)
                            },
                            _ => false
                        };
                    }
                },
                Field::Path => {
                    if let Some(ref val) = expr.val {
                        let file_path = match file_info {
                            Some(ref file_info) => file_info.name.clone(),
                            _ => String::from(entry.path().to_string_lossy())
                        };

                        result = match expr.op {
                            Some(Op::Eq) => {
                                match expr.regex {
                                    Some(ref regex) => regex.is_match(&file_path),
                                    None => val.eq(&file_path)
                                }
                            },
                            Some(Op::Ne) => {
                                match expr.regex {
                                    Some(ref regex) => !regex.is_match(&file_path),
                                    None => val.ne(&file_path)
                                }
                            },
                            Some(Op::Rx) | Some(Op::Like) => {
                                match expr.regex {
                                    Some(ref regex) => regex.is_match(&file_path),
                                    None => false
                                }
                            },
                            Some(Op::Eeq) => {
                                val.eq(&file_path)
                            },
                            Some(Op::Ene) => {
                                val.ne(&file_path)
                            },
                            _ => false
                        };
                    }
                },
                Field::Size | Field::FormattedSize => {
                    if let Some(ref val) = expr.val {
                        let file_size = match file_info {
                            Some(ref file_info) => {
                                Some(file_info.size)
                            },
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);
                                match meta {
                                    Some(ref metadata) => {
                                        Some(metadata.len())
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(file_size) = file_size {
                            let size = parse_filesize(val);
                            if let Some(size) = size {
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => file_size == size,
                                    Some(Op::Ne) | Some(Op::Ene) => file_size != size,
                                    Some(Op::Gt) => file_size > size,
                                    Some(Op::Gte) => file_size >= size,
                                    Some(Op::Lt) => file_size < size,
                                    Some(Op::Lte) => file_size <= size,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Uid => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            let uid = val.parse::<u32>();
                            if let Ok(uid) = uid {
                                let file_uid = mode::get_uid(metadata);
                                if let Some(file_uid) = file_uid {
                                    result = match expr.op {
                                        Some(Op::Eq) | Some(Op::Eeq) => file_uid == uid,
                                        Some(Op::Ne) | Some(Op::Ene) => file_uid != uid,
                                        Some(Op::Gt) => file_uid > uid,
                                        Some(Op::Gte) => file_uid >= uid,
                                        Some(Op::Lt) => file_uid < uid,
                                        Some(Op::Lte) => file_uid <= uid,
                                        _ => false
                                    };
                                }
                            }
                        }
                    }
                },
                Field::User => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            let file_uid = mode::get_uid(metadata);
                            if let Some(file_uid) = file_uid {
                                if let Some(user) = self.user_cache.get_user_by_uid(file_uid) {
                                    let user_name = user.name().to_string_lossy().to_string();
                                    result = match expr.op {
                                        Some(Op::Eq) => {
                                            match expr.regex {
                                                Some(ref regex) => regex.is_match(&user_name),
                                                None => val.eq(&user_name)
                                            }
                                        },
                                        Some(Op::Ne) => {
                                            match expr.regex {
                                                Some(ref regex) => !regex.is_match(&user_name),
                                                None => val.ne(&user_name)
                                            }
                                        },
                                        Some(Op::Rx) | Some(Op::Like) => {
                                            match expr.regex {
                                                Some(ref regex) => regex.is_match(&user_name),
                                                None => false
                                            }
                                        },
                                        Some(Op::Eeq) => {
                                            val.eq(&user_name)
                                        },
                                        Some(Op::Ene) => {
                                            val.ne(&user_name)
                                        },
                                        _ => false
                                    };
                                }
                            }
                        }
                    }
                },
                Field::Gid => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            let gid = val.parse::<u32>();
                            if let Ok(gid) = gid {
                                let file_gid = mode::get_gid(metadata);
                                if let Some(file_gid) = file_gid {
                                    result = match expr.op {
                                        Some(Op::Eq) | Some(Op::Eeq) => file_gid == gid,
                                        Some(Op::Ne) | Some(Op::Ene) => file_gid != gid,
                                        Some(Op::Gt) => file_gid > gid,
                                        Some(Op::Gte) => file_gid >= gid,
                                        Some(Op::Lt) => file_gid < gid,
                                        Some(Op::Lte) => file_gid <= gid,
                                        _ => false
                                    };
                                }
                            }
                        }
                    }
                },
                Field::Group => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            let file_gid = mode::get_gid(metadata);
                            if let Some(file_gid) = file_gid {
                                if let Some(group) = self.user_cache.get_group_by_gid(file_gid) {
                                    let group_name = group.name().to_string_lossy().to_string();
                                    result = match expr.op {
                                        Some(Op::Eq) => {
                                            match expr.regex {
                                                Some(ref regex) => regex.is_match(&group_name),
                                                None => val.eq(&group_name)
                                            }
                                        },
                                        Some(Op::Ne) => {
                                            match expr.regex {
                                                Some(ref regex) => !regex.is_match(&group_name),
                                                None => val.ne(&group_name)
                                            }
                                        },
                                        Some(Op::Rx) | Some(Op::Like) => {
                                            match expr.regex {
                                                Some(ref regex) => regex.is_match(&group_name),
                                                None => false
                                            }
                                        },
                                        Some(Op::Eeq) => {
                                            val.eq(&group_name)
                                        },
                                        Some(Op::Ene) => {
                                            val.ne(&group_name)
                                        },
                                        _ => false
                                    };
                                }
                            }
                        }
                    }
                },
                Field::IsDir => {
                    if let Some(ref val) = expr.val {
                        let is_dir = match file_info {
                            Some(ref file_info) => Some(file_info.name.ends_with('/')),
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);

                                match meta {
                                    Some(ref metadata) => {
                                        Some(metadata.is_dir())
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(is_dir) = is_dir {
                            let bool_val = str_to_bool(val);

                            result = match expr.op {
                                Some(Op::Eq) | Some(Op::Eeq) => {
                                    if bool_val {
                                        is_dir
                                    } else {
                                        !is_dir
                                    }
                                },
                                Some(Op::Ne) | Some(Op::Ene) => {
                                    if bool_val {
                                        !is_dir
                                    } else {
                                        is_dir
                                    }
                                },
                                _ => false
                            };
                        }
                    }
                },
                Field::IsFile => {
                    if let Some(ref val) = expr.val {
                        let is_file = match file_info {
                            Some(ref file_info) => Some(!file_info.name.ends_with('/')),
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);

                                match meta {
                                    Some(ref metadata) => {
                                        Some(metadata.is_file())
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(is_file) = is_file {
                            let bool_val = str_to_bool(val);

                            result = match expr.op {
                                Some(Op::Eq) | Some(Op::Eeq) => {
                                    if bool_val {
                                        is_file
                                    } else {
                                        !is_file
                                    }
                                },
                                Some(Op::Ne) | Some(Op::Ene) => {
                                    if bool_val {
                                        !is_file
                                    } else {
                                        is_file
                                    }
                                },
                                _ => false
                            };
                        }
                    }
                },
                Field::IsSymlink => {
                    if let Some(ref val) = expr.val {
                        let is_symlink = match file_info {
                            Some(_) => Some(false),
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);

                                match meta {
                                    Some(ref metadata) => {
                                        Some(metadata.file_type().is_symlink())
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(is_symlink) = is_symlink {
                            let bool_val = str_to_bool(val);

                            result = match expr.op {
                                Some(Op::Eq) | Some(Op::Eeq) => {
                                    if bool_val {
                                        is_symlink
                                    } else {
                                        !is_symlink
                                    }
                                },
                                Some(Op::Ne) | Some(Op::Ene) => {
                                    if bool_val {
                                        !is_symlink
                                    } else {
                                        is_symlink
                                    }
                                },
                                _ => false
                            };
                        }
                    }
                },
                Field::IsPipe => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_is_pipe);
                    meta = meta_;
                    result = res_;
                },
                Field::IsCharacterDevice => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_is_char_device);
                    meta = meta_;
                    result = res_;
                },
                Field::IsBlockDevice => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_is_block_device);
                    meta = meta_;
                    result = res_;
                },
                Field::IsSocket => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_is_socket);
                    meta = meta_;
                    result = res_;
                },
                Field::Mode => {
                    if let Some(ref val) = expr.val {
                        let mode = match file_info {
                            Some(ref file_info) => {
                                match file_info.mode {
                                    Some(mode) => Some(mode::format_mode(mode)),
                                    _ => None
                                }
                            },
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);

                                match meta {
                                    Some(ref metadata) => {
                                        Some(mode::get_mode(metadata))
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(mode) = mode {
                            result = match expr.op {
                                Some(Op::Eq) => {
                                    match expr.regex {
                                        Some(ref regex) => regex.is_match(&mode),
                                        None => val.eq(&mode)
                                    }
                                },
                                Some(Op::Ne) => {
                                    match expr.regex {
                                        Some(ref regex) => !regex.is_match(&mode),
                                        None => val.ne(&mode)
                                    }
                                },
                                Some(Op::Rx) | Some(Op::Like) => {
                                    match expr.regex {
                                        Some(ref regex) => regex.is_match(&mode),
                                        None => false
                                    }
                                },
                                _ => false
                            };
                        }
                    }
                },
                Field::UserRead => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_user_read);
                    meta = meta_;
                    result = res_;
                },
                Field::UserWrite => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_user_write);
                    meta = meta_;
                    result = res_;
                },
                Field::UserExec => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_user_exec);
                    meta = meta_;
                    result = res_;
                },
                Field::GroupRead => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_group_read);
                    meta = meta_;
                    result = res_;
                },
                Field::GroupWrite => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_group_write);
                    meta = meta_;
                    result = res_;
                },
                Field::GroupExec => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_group_exec);
                    meta = meta_;
                    result = res_;
                },
                Field::OtherRead => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_other_read);
                    meta = meta_;
                    result = res_;
                },
                Field::OtherWrite => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_other_write);
                    meta = meta_;
                    result = res_;
                },
                Field::OtherExec => {
                    let (res_, meta_) = confirm_file_mode(&expr.op, &expr.val, &entry, meta, &file_info, follow_symlinks, &mode::mode_other_exec);
                    meta = meta_;
                    result = res_;
                },
                Field::IsHidden => {
                    if let Some(ref val) = expr.val {
                        let is_hidden = match file_info {
                            Some(ref file_info) => is_hidden(&file_info.name, &None, true),
                            _ => is_hidden(&entry.file_name().to_string_lossy(), &meta, false)
                        };

                        let bool_val = str_to_bool(val);

                        result = match expr.op {
                            Some(Op::Eq) | Some(Op::Eeq) => {
                                if bool_val {
                                    is_hidden
                                } else {
                                    !is_hidden
                                }
                            },
                            Some(Op::Ne) | Some(Op::Ene) => {
                                if bool_val {
                                    !is_hidden
                                } else {
                                    is_hidden
                                }
                            },
                            _ => false
                        };
                    }
                },
                Field::Created => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref _val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            if let Ok(sdt) = metadata.created() {
                                let dt: DateTime<Local> = DateTime::from(sdt);
                                let start = expr.dt_from.unwrap();
                                let finish = expr.dt_to.unwrap();

                                result = match expr.op {
                                    Some(Op::Eeq) => dt == start,
                                    Some(Op::Ene) => dt != start,
                                    Some(Op::Eq) => dt >= start && dt <= finish,
                                    Some(Op::Ne) => dt < start || dt > finish,
                                    Some(Op::Gt) => dt > finish,
                                    Some(Op::Gte) => dt >= start,
                                    Some(Op::Lt) => dt < start,
                                    Some(Op::Lte) => dt <= finish,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Accessed => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref _val) = expr.val {
                        meta = update_meta(entry, meta, follow_symlinks);

                        if let Some(ref metadata) = meta {
                            if let Ok(sdt) = metadata.accessed() {
                                let dt: DateTime<Local> = DateTime::from(sdt);
                                let start = expr.dt_from.unwrap();
                                let finish = expr.dt_to.unwrap();

                                result = match expr.op {
                                    Some(Op::Eeq) => dt == start,
                                    Some(Op::Ene) => dt != start,
                                    Some(Op::Eq) => dt >= start && dt <= finish,
                                    Some(Op::Ne) => dt < start || dt > finish,
                                    Some(Op::Gt) => dt > finish,
                                    Some(Op::Gte) => dt >= start,
                                    Some(Op::Lt) => dt < start,
                                    Some(Op::Lte) => dt <= finish,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Modified => {
                    if let Some(ref _val) = expr.val {
                        let dt = match file_info {
                            Some(ref file_info) => Some(to_local_datetime(&file_info.modified)),
                            _ => {
                                meta = update_meta(entry, meta, follow_symlinks);
                                match meta {
                                    Some(ref metadata) => {
                                        match metadata.modified() {
                                            Ok(sdt) => Some(DateTime::from(sdt)),
                                            _ => None
                                        }
                                    },
                                    _ => None
                                }
                            }
                        };

                        if let Some(dt) = dt {
                            let start = expr.dt_from.unwrap();
                            let finish = expr.dt_to.unwrap();

                            result = match expr.op {
                                Some(Op::Eeq) => dt == start,
                                Some(Op::Ene) => dt != start,
                                Some(Op::Eq) => dt >= start && dt <= finish,
                                Some(Op::Ne) => dt < start || dt > finish,
                                Some(Op::Gt) => dt > finish,
                                Some(Op::Gte) => dt >= start,
                                Some(Op::Lt) => dt < start,
                                Some(Op::Lte) => dt <= finish,
                                _ => false
                            };
                        }
                    }
                },
                Field::HasXattrs => {
                    #[cfg(unix)]
                        {
                            if file_info.is_some() {
                                return (false, meta, dim, mp3)
                            }

                            if let Some(ref val) = expr.val {
                                if let Ok(file) = File::open(&entry.path()) {
                                    if let Ok(xattrs) = file.list_xattr() {
                                        let has_xattrs = xattrs.count() > 0;
                                        let bool_val = str_to_bool(val);

                                        result = match &expr.op {
                                            Some(Op::Eq) | Some(Op::Eeq) => {
                                                if bool_val {
                                                    has_xattrs
                                                } else {
                                                    !has_xattrs
                                                }
                                            },
                                            Some(Op::Ne) | Some(Op::Ene) => {
                                                if bool_val {
                                                    !has_xattrs
                                                } else {
                                                    has_xattrs
                                                }
                                            },
                                            _ => false
                                        };
                                    }
                                }
                            }
                        }
                },
                Field::IsShebang => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    result = is_shebang(&entry.path())
                },
                Field::Width => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if !is_image_dim_readable(&entry.file_name().to_string_lossy()) {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        dim = update_img_dimensions(&entry, dim);

                        if let Some((width, _)) = dim {
                            let val = val.parse::<usize>();
                            if let Ok(val) = val {
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => width == val,
                                    Some(Op::Ne) | Some(Op::Ene) => width != val,
                                    Some(Op::Gt) => width > val,
                                    Some(Op::Gte) => width >= val,
                                    Some(Op::Lt) => width < val,
                                    Some(Op::Lte) => width <= val,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Height => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if !is_image_dim_readable(&entry.file_name().to_string_lossy()) {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        dim = update_img_dimensions(&entry, dim);

                        if let Some((_, height)) = dim {
                            let val = val.parse::<usize>();
                            if let Ok(val) = val {
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => height == val,
                                    Some(Op::Ne) | Some(Op::Ene) => height != val,
                                    Some(Op::Gt) => height > val,
                                    Some(Op::Gte) => height >= val,
                                    Some(Op::Lt) => height < val,
                                    Some(Op::Lte) => height <= val,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Bitrate => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            let val = val.parse::<usize>();
                            if let Ok(val) = val {
                                let bitrate = mp3_meta.frames[0].bitrate as usize;
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => bitrate == val,
                                    Some(Op::Ne) | Some(Op::Ene) => bitrate != val,
                                    Some(Op::Gt) => bitrate > val,
                                    Some(Op::Gte) => bitrate >= val,
                                    Some(Op::Lt) => bitrate < val,
                                    Some(Op::Lte) => bitrate <= val,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Freq => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            let val = val.parse::<usize>();
                            if let Ok(val) = val {
                                let freq = mp3_meta.frames[0].sampling_freq as usize;
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => freq == val,
                                    Some(Op::Ne) | Some(Op::Ene) => freq != val,
                                    Some(Op::Gt) => freq > val,
                                    Some(Op::Gte) => freq >= val,
                                    Some(Op::Lt) => freq < val,
                                    Some(Op::Lte) => freq <= val,
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Title => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            if let Some(ref mp3_tag) = mp3_meta.tag {
                                let title = &mp3_tag.title;
                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(title),
                                            None => val.eq(title)
                                        }
                                    },
                                    Some(Op::Ne) | Some(Op::Ene) => {
                                        match expr.regex {
                                            Some(ref regex) => !regex.is_match(title),
                                            None => val.ne(title)
                                        }
                                    },
                                    Some(Op::Rx) | Some(Op::Like) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(title),
                                            None => false
                                        }
                                    },
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Artist => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            if let Some(ref mp3_tag) = mp3_meta.tag {
                                let artist = &mp3_tag.artist;

                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(artist),
                                            None => val.eq(artist)
                                        }
                                    },
                                    Some(Op::Ne) | Some(Op::Ene) => {
                                        match expr.regex {
                                            Some(ref regex) => !regex.is_match(artist),
                                            None => val.ne(artist)
                                        }
                                    },
                                    Some(Op::Rx) | Some(Op::Like) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(artist),
                                            None => false
                                        }
                                    },
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Album => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            if let Some(ref mp3_tag) = mp3_meta.tag {
                                let album = &mp3_tag.album;

                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(album),
                                            None => val.eq(album)
                                        }
                                    },
                                    Some(Op::Ne) | Some(Op::Ene) => {
                                        match expr.regex {
                                            Some(ref regex) => !regex.is_match(album),
                                            None => val.ne(album)
                                        }
                                    },
                                    Some(Op::Rx) | Some(Op::Like) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(album),
                                            None => false
                                        }
                                    },
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::Year => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            let val = val.parse::<usize>();
                            if let Ok(val) = val {
                                if let Some(ref mp3_tag) = mp3_meta.tag {
                                    let year = mp3_tag.year as usize;
                                    if year > 0 {
                                        result = match expr.op {
                                            Some(Op::Eq) | Some(Op::Eeq) => year == val,
                                            Some(Op::Ne) | Some(Op::Ene) => year != val,
                                            Some(Op::Gt) => year > val,
                                            Some(Op::Gte) => year >= val,
                                            Some(Op::Lt) => year < val,
                                            Some(Op::Lte) => year <= val,
                                            _ => false
                                        };
                                    }
                                }
                            }
                        }
                    }
                },
                Field::Genre => {
                    if file_info.is_some() {
                        return (false, meta, dim, mp3)
                    }

                    if let Some(ref val) = expr.val {
                        mp3 = update_mp3_meta(&entry, mp3);

                        if let Some(ref mp3_meta) = mp3 {
                            if let Some(ref mp3_tag) = mp3_meta.tag {
                                let genre = &format!("{:?}", &mp3_tag.genre);

                                result = match expr.op {
                                    Some(Op::Eq) | Some(Op::Eeq) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(genre),
                                            None => val.eq(genre)
                                        }
                                    },
                                    Some(Op::Ne) | Some(Op::Ene) => {
                                        match expr.regex {
                                            Some(ref regex) => !regex.is_match(genre),
                                            None => val.ne(genre)
                                        }
                                    },
                                    Some(Op::Rx) | Some(Op::Like) => {
                                        match expr.regex {
                                            Some(ref regex) => regex.is_match(genre),
                                            None => false
                                        }
                                    },
                                    _ => false
                                };
                            }
                        }
                    }
                },
                Field::IsArchive => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_archive);
                },
                Field::IsAudio => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_audio);
                },
                Field::IsBook => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_book);
                },
                Field::IsDoc => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_doc);
                },
                Field::IsImage => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_image);
                },
                Field::IsSource => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_source);
                },
                Field::IsVideo => {
                    result = confirm_file_ext(&expr.op, &expr.val, &entry, &file_info, &is_video);
                }
            }
        }

        (result, meta, dim, mp3)
    }
}

fn confirm_file_mode(expr_op: &Option<Op>,
                     expr_val: &Option<String>,
                     entry: &DirEntry,
                     meta: Option<Box<Metadata>>,
                     file_info: &Option<FileInfo>,
                     follow_symlinks: bool,
                     mode_func: &Fn(u32) -> bool) -> (bool, Option<Box<Metadata>>) {
    let mut result = false;
    let mut meta = meta;

    if let Some(ref val) = expr_val {
        let mode = match file_info {
            Some(ref file_info) => file_info.mode,
            _ => {
                meta = update_meta(entry, meta, follow_symlinks);

                match meta {
                    Some(ref metadata) => mode::get_mode_from_boxed_unix_int(metadata),
                    _ => None
                }
            }
        };

        if let Some(mode) = mode {
            let bool_val = str_to_bool(val);

            result = match expr_op {
                Some(Op::Eq) => {
                    if bool_val {
                        mode_func(mode)
                    } else {
                        !mode_func(mode)
                    }
                },
                Some(Op::Ne) => {
                    if bool_val {
                        !mode_func(mode)
                    } else {
                        mode_func(mode)
                    }
                },
                _ => false
            };
        }
    }

    (result, meta)
}

fn confirm_file_ext(expr_op: &Option<Op>,
                    expr_val: &Option<String>,
                    entry: &DirEntry,
                    file_info: &Option<FileInfo>,
                    file_ext_func: &Fn(&str) -> bool) -> bool {
    let mut result = false;

    if let Some(ref val) = expr_val {
        let file_name = match file_info {
            Some(ref file_info) => file_info.name.clone(),
            _ => String::from(entry.file_name().to_string_lossy())
        };

        let bool_val = str_to_bool(val);

        result = match expr_op {
            Some(Op::Eq) | Some(Op::Eeq) => {
                if bool_val {
                    file_ext_func(&file_name)
                } else {
                    !file_ext_func(&file_name)
                }
            },
            Some(Op::Ne) | Some(Op::Ene) => {
                if bool_val {
                    !file_ext_func(&file_name)
                } else {
                    file_ext_func(&file_name)
                }
            },
            _ => false
        };
    }

    result
}

fn update_meta(entry: &DirEntry, meta: Option<Box<Metadata>>, follow_symlinks: bool) -> Option<Box<Metadata>> {
    if !meta.is_some() {
        let metadata = match follow_symlinks {
            false => symlink_metadata(entry.path()),
            true => fs::metadata(entry.path())
        };

        if let Ok(metadata) = metadata {
            return Some(Box::new(metadata));
        }
    }

    meta
}

fn update_img_dimensions(entry: &DirEntry, dim: Option<(usize, usize)>) -> Option<(usize, usize)> {
    match dim {
        None => {
            match imagesize::size(entry.path()) {
                Ok(dimensions) => Some((dimensions.width, dimensions.height)),
                _ => None
            }
        },
        Some(dim_) => Some(dim_)
    }
}

fn update_mp3_meta(entry: &DirEntry, mp3: Option<MP3Metadata>) -> Option<MP3Metadata> {
    match mp3 {
        None => {
            match mp3_metadata::read_from_file(entry.path()) {
                Ok(mp3_meta) => Some(mp3_meta),
                _ => None
            }
        },
        Some(mp3_) => Some(mp3_)
    }
}

fn is_shebang(path: &PathBuf) -> bool {
    if let Ok(file) = File::open(path) {
        let mut buf_reader = BufReader::new(file);
        let mut buf = vec![0; 2];
        if buf_reader.read_exact(&mut buf).is_ok() {
            return buf[0] == 0x23 && buf[1] == 0x21
        }
    }

    false
}

#[allow(unused)]
fn is_hidden(file_name: &str, metadata: &Option<Box<Metadata>>, archive_mode: bool) -> bool {
    if archive_mode {
        if !file_name.contains('\\') {
            return parse_unix_filename(file_name).starts_with('.');
        } else {
            return false;
        }
    }

    #[cfg(unix)]
    {
        return file_name.starts_with('.');
    }

    #[cfg(windows)]
    {
        if let Some(ref metadata) = metadata {
            return mode::get_mode(metadata).contains("Hidden");
        }
    }

    #[cfg(not(unix))]
    {
        false
    }
}

macro_rules! def_extension_queries {
    ($($name:ident $extensions:expr);*) => {
        $(
            fn $name(file_name: &str) -> bool {
                has_extension(file_name, &$extensions)
            }
        )*
    }
}

def_extension_queries! {
    is_zip_archive          [".zip", ".jar", ".war", ".ear"]
;   is_archive              [".7z", ".bz2", ".bzip2", ".gz", ".gzip", ".rar", ".tar", ".xz", ".zip"]
;   is_audio                [".aac", ".aiff", ".amr", ".flac", ".gsm", ".m4a", ".m4b", ".m4p", ".mp3", ".ogg", ".wav", ".wma"]
;   is_book                 [".azw3", ".chm", ".epub", ".fb2", ".mobi", ".pdf"]
;   is_doc                  [".accdb", ".doc", ".docm", ".docx", ".dot", ".dotm", ".dotx", ".mdb", ".ods", ".odt", ".pdf", ".potm", ".potx", ".ppt", ".pptm", ".pptx", ".rtf", ".xlm", ".xls", ".xlsm", ".xlsx", ".xlt", ".xltm", ".xltx", ".xps"]
;   is_image                [".bmp", ".gif", ".jpeg", ".jpg", ".png", ".tiff", ".webp"]
;   is_image_dim_readable   [".bmp", ".gif", ".jpeg", ".jpg", ".png", ".webp"]
;   is_source               [".asm", ".c", ".cpp", ".cs", ".go", ".h", ".hpp", ".java", ".js", ".jsp", ".pas", ".php", ".pl", ".pm", ".py", ".rb", ".rs", ".swift"]
;   is_video                [".3gp", ".avi", ".flv", ".m4p", ".m4v", ".mkv", ".mov", ".mp4", ".mpeg", ".mpg", ".webm", ".wmv"]
}

fn has_extension(file_name: &str, extensions: &[&str]) -> bool {
    let s = file_name.to_ascii_lowercase();

    for ext in extensions {
        if s.ends_with(ext) {
            return true
        }
    }

    false
}

#[cfg(windows)]
use std;
#[cfg(windows)]
use std::ffi::OsStr;

#[cfg(windows)]
struct UsersCache;

#[cfg(windows)]
impl UsersCache {
    fn new() -> Self {
        UsersCache { }
    }

    fn get_user_by_uid(&self, _: u32) -> Option< std::sync::Arc<User>> {
        None
    }

    fn get_group_by_gid(&self, _: u32) -> Option< std::sync::Arc<Group>> {
        None
    }
}

#[cfg(windows)]
struct User;

#[cfg(windows)]
impl User {
    fn name(&self) -> &OsStr {
        "".as_ref()
    }
}

#[cfg(windows)]
struct Group;

#[cfg(windows)]
impl Group {
    fn name(&self) -> &OsStr {
        "".as_ref()
    }
}
