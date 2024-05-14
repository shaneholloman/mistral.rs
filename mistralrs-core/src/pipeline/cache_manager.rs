use std::sync::{Arc, Mutex, MutexGuard};

use candle_core::Tensor;

use crate::{get_mut_arcmutex, sequence::Sequence, Pipeline};

pub trait CacheManager {
    fn clone_in_cache(
        &self,
        pipeline: &mut dyn Pipeline,
        seqs: &mut [&mut Sequence],
        modify_draft_cache: bool,
    );
    fn clone_out_cache(
        &self,
        pipeline: &mut dyn Pipeline,
        seqs: &mut [&mut Sequence],
        modify_draft_cache: bool,
    );
    fn set_none_cache(&self, pipeline: &mut dyn Pipeline, modify_draft_cache: bool);
}

pub type LayerCaches = Vec<Option<(Tensor, Tensor)>>;

#[derive(Debug, Clone)]
pub struct Cache {
    cache: Arc<Mutex<LayerCaches>>,
    xlora_cache: Option<Arc<Mutex<LayerCaches>>>,
    draft_cache: Arc<Mutex<LayerCaches>>,
    scalings_cache: Option<Arc<Mutex<Option<Tensor>>>>,
}

impl Cache {
    pub(crate) fn new(len: usize, is_xlora: bool) -> Self {
        Self {
            cache: Arc::new(Mutex::new(vec![None; len])),
            xlora_cache: if is_xlora {
                Some(Arc::new(Mutex::new(vec![None; len])))
            } else {
                None
            },
            draft_cache: Arc::new(Mutex::new(vec![None; len])),
            scalings_cache: if is_xlora {
                Some(Arc::new(Mutex::new(None)))
            } else {
                None
            },
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, LayerCaches> {
        get_mut_arcmutex!(self.cache)
    }

    pub(crate) fn draft_lock(&self) -> MutexGuard<'_, LayerCaches> {
        get_mut_arcmutex!(self.draft_cache)
    }

    /// # Panics
    /// If there is no xlora cache
    pub(crate) fn xlora_lock(&self) -> MutexGuard<'_, LayerCaches> {
        get_mut_arcmutex!(self.xlora_cache.as_ref().expect("No X-LoRA cache."))
    }

    /// # Panics
    /// If there is no xlora cache
    pub(crate) fn get_scalings_cache(&self) -> MutexGuard<'_, Option<Tensor>> {
        get_mut_arcmutex!(self
            .scalings_cache
            .as_ref()
            .expect("No X-LoRA scalings cache."))
    }

    pub(crate) fn is_xlora(&self) -> bool {
        self.xlora_cache.is_some()
    }
}

pub struct DefaultCacheManager;

enum SeqCache {
    Normal,
    XLora,
    Draft,
}

fn clone_in_cache(
    num_hidden_layers: usize,
    cache: &mut LayerCaches,
    seqs: &mut [&mut crate::sequence::Sequence],
    src: SeqCache,
) {
    let mut new_cache = Vec::new();
    for layer in 0..num_hidden_layers {
        let mut k_vec = Vec::new();
        let mut v_vec = Vec::new();
        for seq in &mut *seqs {
            let src_cache = match src {
                SeqCache::Normal => seq.cache(),
                SeqCache::XLora => seq.xlora_cache(),
                SeqCache::Draft => seq.draft_cache(),
            };
            let cache = src_cache.get(layer).unwrap();
            let cache = cache
                .as_ref()
                .expect("Not handling completions in `clone_in_cache`.");
            k_vec.push(cache.0.clone());
            v_vec.push(cache.1.clone());
        }
        new_cache.push(Some((
            if k_vec.len() > 1 {
                Tensor::cat(&k_vec, 0).unwrap()
            } else {
                k_vec[0].clone()
            },
            if v_vec.len() > 1 {
                Tensor::cat(&v_vec, 0).unwrap()
            } else {
                v_vec[0].clone()
            },
        )));
    }
    *cache = new_cache;
}

fn clone_out_cache(
    num_hidden_layers: usize,
    cache: &mut LayerCaches,
    seqs: &mut [&mut crate::sequence::Sequence],
    target: SeqCache,
) {
    for layer in 0..num_hidden_layers {
        let cache = cache.get(layer).unwrap();
        let k_cache = cache.as_ref().unwrap().0.clone();
        let v_cache = cache.as_ref().unwrap().1.clone();

        let k_caches = k_cache.chunk(seqs.len(), 0).unwrap();
        debug_assert_eq!(k_caches.len(), seqs.len());
        let v_caches = v_cache.chunk(seqs.len(), 0).unwrap();
        debug_assert_eq!(v_caches.len(), seqs.len());

        for (seq_i, seq) in seqs.iter_mut().enumerate() {
            let output_cache = match target {
                SeqCache::Normal => seq.cache(),
                SeqCache::XLora => seq.xlora_cache(),
                SeqCache::Draft => seq.draft_cache(),
            };
            let seq_cache = &mut output_cache[layer];
            let k = k_caches.get(seq_i).unwrap().clone();
            let v = v_caches.get(seq_i).unwrap().clone();
            *seq_cache = Some((k, v));
        }
    }
}

impl CacheManager for DefaultCacheManager {
    fn clone_in_cache(
        &self,
        pipeline: &mut dyn Pipeline,
        seqs: &mut [&mut crate::sequence::Sequence],
        modify_draft_cache: bool,
    ) {
        if modify_draft_cache {
            clone_in_cache(
                pipeline.get_metadata().num_hidden_layers,
                &mut pipeline.cache().lock(),
                seqs,
                SeqCache::Draft,
            );
            return;
        }
        clone_in_cache(
            pipeline.get_metadata().num_hidden_layers,
            &mut pipeline.cache().lock(),
            seqs,
            SeqCache::Normal,
        );
        if pipeline.get_metadata().is_xlora && !pipeline.get_metadata().has_no_kv_cache {
            clone_in_cache(
                pipeline.get_metadata().num_hidden_layers,
                &mut pipeline.cache().xlora_lock(),
                seqs,
                SeqCache::XLora,
            );
        }
        if pipeline.get_metadata().is_xlora {
            pipeline
                .cache()
                .get_scalings_cache()
                .clone_from(seqs[0].scaling_cache());
        }
    }

    fn clone_out_cache(
        &self,
        pipeline: &mut dyn Pipeline,
        seqs: &mut [&mut crate::sequence::Sequence],
        modify_draft_cache: bool,
    ) {
        if modify_draft_cache {
            clone_out_cache(
                pipeline.get_metadata().num_hidden_layers,
                &mut pipeline.cache().lock(),
                seqs,
                SeqCache::Draft,
            );
            return;
        }
        clone_out_cache(
            pipeline.get_metadata().num_hidden_layers,
            &mut pipeline.cache().lock(),
            seqs,
            SeqCache::Normal,
        );
        if pipeline.get_metadata().is_xlora && !pipeline.get_metadata().has_no_kv_cache {
            clone_out_cache(
                pipeline.get_metadata().num_hidden_layers,
                &mut pipeline.cache().xlora_lock(),
                seqs,
                SeqCache::XLora,
            );
        }
        if pipeline.get_metadata().is_xlora {
            seqs[0]
                .scaling_cache()
                .clone_from(&pipeline.cache().get_scalings_cache());
        }
    }

    fn set_none_cache(&self, pipeline: &mut dyn Pipeline, modify_draft_cache: bool) {
        let mut new_cache = Vec::new();
        for _ in 0..pipeline.get_metadata().num_hidden_layers {
            new_cache.push(None);
        }
        pipeline.cache().lock().clone_from(&new_cache);
        if modify_draft_cache {
            pipeline.cache().draft_lock().clone_from(&new_cache);
        }
        if pipeline.cache().is_xlora() {
            *pipeline.cache().xlora_lock() = new_cache;
        }
    }
}
