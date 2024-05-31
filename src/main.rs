use core::fmt;
use std::{collections::HashMap, fmt::Display, io, process, time::Instant};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use rayon::iter::{ParallelBridge, ParallelIterator};
use rustyline::{config::Configurer, DefaultEditor};
use tantivy::{
    collector::TopDocs,
    query::QueryParser,
    schema::{
        Field, FieldValue, IndexRecordOption, OwnedValue, Schema, TextFieldIndexing, Value,
        INDEXED, STORED, TEXT,
    },
    tokenizer::TextAnalyzer,
    Document, IndexWriter, Opstamp, TantivyDocument,
};
use walkdir::WalkDir;

const AUDIO_EXT: phf::Set<&'static str> = phf::phf_set! {
    // trash
    "mp3",

    // open codecs/containers
    "flac",
    "opus",
    "ape",
    "ogg",
    "mka",
    "webm",

    // apple stuff
    "aac",
    "alac",
    "m4a",
    "caf",

    // windows stuff
    "wma",
    "wav",
};

#[derive(Default, Debug)]
struct AudioFile {
    /// displayed (but only index the filename)
    file_path: Utf8PathBuf,

    /// normally the same as artist, should be indexed but only displayed as fallback
    album_artist: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    title: Option<String>,
    /// if a part of an album this is the track number within that album
    track: Option<u64>,
    date: Option<String>,

    /// keys are first lowercased
    extras: HashMap<String, String>,
}

impl AudioFile {
    fn new(path: Utf8PathBuf) -> Self {
        let mut this = Self::default();

        this.file_path = path;

        this
    }

    fn place(&mut self, key: impl Into<String> + AsRef<str>, value: impl Into<String>) {
        let k = key.as_ref().to_lowercase();
        let value = value.into();

        match &*k {
            "album_artist" => self.album_artist = Some(value),
            "artist" => self.artist = Some(value),
            "album" => self.album = Some(value),
            "title" => self.title = Some(value),
            "track" => {
                let i = value.split_once('/').map_or(&*value, |(n, _total)| n);

                if let Ok(n) = i.parse() {
                    self.track = Some(n);
                }
            }
            "date" => self.date = Some(value),

            _ => {
                self.extras.insert(k, value);
            }
        }
    }

    fn from_kv_and_path<'a>(
        path: impl Into<Utf8PathBuf>,
        kv: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        let mut this = Self::new(path.into());

        for (k, v) in kv {
            this.place(k, v);
        }

        this
    }

    fn tantivy_store(&self, scm: &HardSchema) -> TantivyDocument {
        let mut doc = TantivyDocument::new();

        doc.add_text(scm.path, &self.file_path);

        if let Some(artist) = self.artist.as_ref().or(self.album_artist.as_ref()) {
            doc.add_text(scm.artist, artist);
        }

        if let Some(album) = &self.album {
            doc.add_text(scm.album, album);
        }

        if let Some(title) = &self.title {
            doc.add_text(scm.title, title);
        }

        if let Some(track) = self.track {
            doc.add_u64(scm.track, track);
        }

        if let Some(date) = &self.date {
            doc.add_text(scm.date, date);
        }

        doc.add_text(
            scm.extras,
            self.extras
                .values()
                .map(|s| &**s)
                .collect::<Vec<&str>>()
                .join(" "),
        );

        doc
    }

    fn store_fieldvalue(&mut self, scm: &HardSchema, fv: &FieldValue) {
        let f = &fv.field;

        fn must_string(v: &OwnedValue) -> String {
            let OwnedValue::Str(s) = v else {
                unreachable!("this field must be a string")
            };

            s.to_owned()
        }

        fn must_u64(v: &OwnedValue) -> u64 {
            let &OwnedValue::U64(v) = v else {
                unreachable!("this field must be a u64")
            };

            v
        }

        #[deny(unused_variables)]
        let HardSchema {
            path,
            artist,
            album,
            title,
            track,
            date,
            extras,
        } = scm;

        _ = extras;

        match f {
            _ if f == path => self.file_path = must_string(&fv.value).into(),
            _ if f == artist => self.artist = Some(must_string(&fv.value)),
            _ if f == album => self.album = Some(must_string(&fv.value)),
            _ if f == title => self.title = Some(must_string(&fv.value)),
            _ if f == track => self.track = Some(must_u64(&fv.value)),
            _ if f == date => self.date = Some(must_string(&fv.value)),

            _ => (),
        }
    }

    fn tantivy_recall(scm: &HardSchema, doc: &TantivyDocument) -> Self {
        let mut s = Self::new(Utf8PathBuf::new());

        for itm in doc.field_values() {
            s.store_fieldvalue(scm, itm);
        }

        s
    }
}

impl Display for AudioFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // file name must exist to be a valid AudioFile
        let fname = self.file_path.file_name().unwrap();

        write!(f, "{fname}")?;

        if let Some(title) = &self.title {
            write!(f, ": {title}")?;
        }

        if let Some(artist) = self.artist.as_ref().or(self.album_artist.as_ref()) {
            write!(f, " - {artist}")?;
        }

        if let Some(album) = &self.album {
            write!(f, " - {album}")?;
        }

        if let Some(date) = &self.date {
            write!(f, " ({date})")?;
        }

        if let Some(track) = self.track {
            write!(f, " #{track}")?;
        }

        Ok(())
    }
}

struct HardSchema {
    path: Field,
    artist: Field,
    album: Field,
    title: Field,
    track: Field,
    date: Field,
    extras: Field,
}

impl HardSchema {
    const PATH: &'static str = "path";
    const ARTIST: &'static str = "artist";
    const ALBUM: &'static str = "album";
    const TITLE: &'static str = "title";
    const TRACK: &'static str = "track";
    const DATE: &'static str = "date";
    const EXTRAS: &'static str = "extras";

    fn schema() -> (Schema, Self) {
        let mut schema = Schema::builder();

        let text = TEXT.set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("ngram3")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let text_stored = text.clone() | STORED;

        schema.add_text_field(HardSchema::PATH, text_stored.clone());
        schema.add_text_field(HardSchema::ARTIST, text_stored.clone());
        schema.add_text_field(HardSchema::ALBUM, text_stored.clone());
        schema.add_text_field(HardSchema::TITLE, text_stored.clone());
        schema.add_u64_field(HardSchema::TRACK, INDEXED | STORED);
        schema.add_text_field(HardSchema::DATE, text_stored.clone());
        schema.add_text_field(HardSchema::EXTRAS, text);

        let scm = schema.build();

        let hard = Self::from_schema(&scm);

        (scm, hard)
    }

    fn all(&self) -> Vec<Field> {
        vec![
            self.path,
            self.artist,
            self.album,
            self.title,
            self.track,
            self.date,
            self.extras,
        ]
    }

    fn from_schema(schema: &Schema) -> Self {
        // none of these will panic when used on the schema generated by tantivy_schema
        Self {
            path: schema.get_field(HardSchema::PATH).unwrap(),
            artist: schema.get_field(HardSchema::ARTIST).unwrap(),
            album: schema.get_field(HardSchema::ALBUM).unwrap(),
            title: schema.get_field(HardSchema::TITLE).unwrap(),
            track: schema.get_field(HardSchema::TRACK).unwrap(),
            date: schema.get_field(HardSchema::DATE).unwrap(),
            extras: schema.get_field(HardSchema::EXTRAS).unwrap(),
        }
    }
}

fn recursive_find_audiofiles(
    subdir: &Utf8Path,
) -> impl ParallelIterator<Item = io::Result<AudioFile>> {
    WalkDir::new(subdir)
        .follow_links(true)
        .into_iter()
        .par_bridge()
        .filter(|p| p.as_ref().map_or(true, |f| f.file_type().is_file()))
        .map(|res| {
            let file = res?;

            let path = Utf8PathBuf::try_from(file.into_path()).map_err(|e| e.into_io_error())?;

            let Some(ext) = path.extension() else {
                return Err(io::Error::other("not an audio file"));
            };

            if !AUDIO_EXT.contains(ext) {
                return Err(io::Error::other("not an audio file"));
            }

            // do allocation after we checked its an audio file
            let path = path.canonicalize_utf8()?;

            let ffmpeg_meta = ffmpeg_next::format::input(&path)?;

            // metadata() is coming from a private Deref<Target = Context> type...
            // TODO PR it to not be like this
            Ok(AudioFile::from_kv_and_path(
                path,
                ffmpeg_meta.metadata().iter(),
            ))
        })
}

#[derive(clap::Parser)]
/// A music search engine utilizing ffmpeg and tantivy to gather and query songs
struct Args {
    /// dirs to recurse into to find music
    #[arg(num_args = 1..)]
    dir: Vec<Utf8PathBuf>,
}

struct Hyperlink<H: Display, T: Display> {
    hyperlink: H,
    text: T,
}

impl<H: Display, T: Display> Hyperlink<H, T> {
    fn new(hyperlink: H, text: T) -> Self {
        Self { hyperlink, text }
    }
}

impl<H: Display, T: Display> Display for Hyperlink<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            self.hyperlink, self.text
        )
    }
}

fn main() {
    let args = Args::parse();

    let (scm, map) = HardSchema::schema();

    let index = tantivy::Index::create_in_ram(scm.clone());

    let hostname_own = gethostname::gethostname();
    let hostname = hostname_own.to_str().unwrap_or("");

    index.tokenizers().register(
        "ngram3",
        TextAnalyzer::builder(
            tantivy::tokenizer::NgramTokenizer::new(3, 3, false)
                .expect("this tokenizer will not error with these arguments"),
        )
        .filter(tantivy::tokenizer::LowerCaser)
        .build(),
    );
    let mut writer = index
        .writer(50_000_000)
        .expect("this writer will not error with 50mb of storage allocated");

    for dir in &args.dir {
        recursive_find_audiofiles(dir)
            .map(|v| v.map(|f| writer.add_document(f.tantivy_store(&map))))
            .for_each(|_| ());
    }

    writer.commit().unwrap();

    drop(writer);

    let mut editor = DefaultEditor::new().unwrap();
    editor.set_auto_add_history(true);
    editor.set_completion_type(rustyline::CompletionType::List);

    // unwrap possibly safe because this is ram backed, docs are unclear
    let reader = index.reader().unwrap();

    let qp = QueryParser::for_index(&index, map.all());

    loop {
        let line = match editor.readline("> ") {
            Ok(line) => line,
            Err(_) => break,
        };

        let q = qp.parse_query_lenient(&line).0;

        let start = Instant::now();

        let search = reader.searcher();
        let top_resp = search.search(&q, &TopDocs::with_limit(10)).unwrap();

        for (_, address) in top_resp.into_iter().rev() {
            let retr = AudioFile::tantivy_recall(&map, &search.doc(address).unwrap());

            println!(
                "{}",
                Hyperlink::new(format_args!("file://{hostname}{}", retr.file_path), &retr)
            );
        }

        if !line.is_empty() {
            println!("searched in {:?}", start.elapsed());
        }
    }
}
