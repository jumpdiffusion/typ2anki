use once_cell::sync::OnceCell;
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};
use typst::{
    syntax::{FileId, Source, VirtualPath, RootedPath, VirtualRoot},
};

use crate::{
    anki_api,
    card_wrapper::{CardInfo, CardModificationStatus, TFiles},
    cards_cache::CardsCacheManager,
    config, generator,
    output::{OutputCompiledCardInfo, OutputManager, OutputMessage},
    typst_as_library::{self, DiagnosticFormat, DownloadLocks},
    utils,
};

// A cache_manager should be passed so that in the case of an error during
// compilation or upload, the card's hash can be removed from the cache.
pub fn compile_cards_concurrent(
    cards: &Vec<CardInfo>,
    output: Arc<impl OutputManager + 'static>,
    cache_manager: Arc<Mutex<CardsCacheManager>>,
    file_stats: TFiles,
) {
    let cfg = config::get();
    if cfg.generation_concurrency <= 1 {
        compile_cards(cards, output, cache_manager, file_stats);
        return;
    }

    let total = cards.len();
    if total > 0 {
        let n_batches = std::cmp::min(cfg.generation_concurrency, total);
        let chunk_size = total.div_ceil(n_batches);

        let mut handles = Vec::with_capacity(n_batches);
        for i in 0..n_batches {
            let start = i * chunk_size;
            let end = ((i + 1) * chunk_size).min(total);
            let batch = cards[start..end].to_vec();
            let output_clone = output.clone();
            let cache_manager_clone = cache_manager.clone();
            let file_stats_clone = file_stats.clone();
            let handle = std::thread::spawn(move || {
                compile_cards(&batch, output_clone, cache_manager_clone, file_stats_clone);
            });
            handles.push(handle);
        }

        for h in handles {
            let _ = h.join();
        }
    }
}

static TYPST_PACKAGE_DOWNLOAD_LOCK: OnceCell<DownloadLocks> = OnceCell::new();

pub fn compile_cards(
    cards: &Vec<CardInfo>,
    output: Arc<impl OutputManager + 'static>,
    cache_manager: Arc<Mutex<CardsCacheManager>>,
    file_stats: TFiles,
) {
    if cards.is_empty() {
        return;
    }
    let cfg = config::get();

    let uploader = anki_api::CardUploaderThread::new();

    let mut world = typst_as_library::TypstWrapperWorld::new_with_download_locks(
        cfg.path.to_string_lossy().into_owned(),
        "".to_string(),
        &cfg.typst_input,
        TYPST_PACKAGE_DOWNLOAD_LOCK
            .get_or_init(DownloadLocks::default)
            .clone(),
    );
    world.output_manager = Some(output.clone());


    let card_error = |card: &CardInfo, m: OutputMessage| {
        let mut cache_manager = cache_manager.lock().unwrap();
        cache_manager.remove_card_hash(card.deck_name.as_str(), &card.card_id);

        {
            let mut file_stats = file_stats.write().unwrap();
            if let Some(stats) = file_stats.get_mut(&card.source_file) {
                match card.modification_status {
                    CardModificationStatus::New => stats.new_cards.1 += 1,
                    CardModificationStatus::Updated => stats.updated_cards.1 += 1,
                    CardModificationStatus::Unchanged => stats.unchanged_cards.1 += 1,
                    CardModificationStatus::Unknown => {}
                }
            }
        }

        output.send(m);
    };

    // Returns a Result with Option of front and back HTML strings
    let mut compile_card = |card: &CardInfo| -> Result<Option<(String, String)>, String> {
        if card.modification_status == CardModificationStatus::Unchanged {
            output.send(OutputMessage::SkipCompileCard(card.into()));
            return Ok(None);
        }
        
        let mut compile_side = |side: &str| -> Result<String, String> {
            let base = generator::generate_card_file_content(
                card.relative_ankiconf_path(),
                "".to_string(),
                side,
            );
            world.source = Source::new(
                FileId::new(RootedPath::new(VirtualRoot::Project, VirtualPath::new(&card.path_relative_to_root()).unwrap())),
                base.clone(),
            );
            world.source.edit(base.len()..base.len(), &card.content);

            let out = typst::compile(&world);
            let document = out.output.map_err(|e| {
                typst_as_library::render_diagnostics(
                    &world,
                    e.as_slice(),
                    out.warnings.as_slice(),
                    DiagnosticFormat::Human,
                )
                .unwrap_or_else(|_| "Failed to render diagnostics.".to_string())
            })?;

            typst_html::html(&document, &typst_html::HtmlOptions::default())
                .map_err(|e| format!("Error generating HTML for {} side: {:?}", side, e))
        };

        let front_html = compile_side("front")?;
        let back_html = compile_side("back")?;

        output.send(OutputMessage::CompiledCard(card.into()));

        Ok(Some((front_html, back_html)))
    };

    for card in cards {
        match compile_card(card) {
            Ok(Some((front_b64, back_b64))) => {
                if let Err(e) = uploader.upload_card(card, &front_b64, &back_b64) {
                    card_error(
                        card,
                        OutputMessage::PushError(OutputCompiledCardInfo::build(
                            card,
                            Some(format!("Error uploading card to Anki: {}", e)),
                        )),
                    );
                } else {
                    output.send(OutputMessage::PushedCard(card.into()));
                }
            }
            Ok(None) => {}
            Err(msg) => {
                card_error(
                    card,
                    OutputMessage::CompileError(OutputCompiledCardInfo::build(card, Some(msg))),
                );
            }
        }
    }
}
