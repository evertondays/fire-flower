//! Busca por frase: confere as posições dos termos em cada documento.

use crate::Result;
use crate::engine::view::Source;
use crate::DocId;
use crate::codec::PositionsCursor;
use crate::engine::keys::POS;
use crate::engine::keys::Key;
use crate::Error;
use crate::text::Clause;
use crate::engine::view::View;

impl View<'_> {
    pub(crate) fn check_phrases(&self, clauses: &[Clause]) -> Result<()> {
        if !self.engine.positions && clauses.iter().any(|c| matches!(c, Clause::Phrase(_))) {
            return Err(Error::Config("banco sem posições: busca por frase indisponível".into()));
        }
        Ok(())
    }

    /// As listas `pos:` dos termos de uma frase, na ordem dos termos.
    pub(crate) fn load_positions(&self, phrase: &[(u32, u64)]) -> Result<Vec<rocksdb::DBPinnableSlice<'_>>> {
        phrase
            .iter()
            .map(|&(_, node)| {
                self.snap
                    .get_bytes(Key::new(POS, node).as_ref())?
                    .ok_or_else(|| Error::Corrupt(format!("termo do nó {node} sem posições")))
            })
            .collect()
    }
}

/// Confere uma frase documento a documento (em ordem crescente).
pub(crate) struct PhraseMatcher<'s> {
    pub(crate) offsets: Vec<u32>,
    pub(crate) cursors: Vec<PositionsCursor<'s>>,
    pub(crate) positions: Vec<Vec<u32>>,
    pub(crate) at: Vec<usize>,
}

impl<'s> PhraseMatcher<'s> {
    pub(crate) fn new(phrase: &[(u32, u64)], slices: &'s [rocksdb::DBPinnableSlice<'_>]) -> Result<Self> {
        let cursors = slices.iter().map(|s| PositionsCursor::new(s)).collect::<Result<_>>()?;
        Ok(Self {
            offsets: phrase.iter().map(|&(o, _)| o).collect(),
            cursors,
            positions: vec![Vec::new(); phrase.len()],
            at: vec![0; phrase.len()],
        })
    }

    /// Quantas vezes a frase aparece em `doc`, e quantas dessas no título
    /// (as que começam antes de `title_span`). `first_only` para na primeira.
    pub(crate) fn count(&mut self, doc: DocId, first_only: bool, title_span: u32) -> Result<(u32, u32)> {
        for (cursor, positions) in self.cursors.iter_mut().zip(&mut self.positions) {
            if !cursor.seek(doc, positions)? {
                return Ok((0, 0));
            }
        }
        Ok(count_phrase(&self.offsets, &self.positions, &mut self.at, first_only, title_span))
    }
}

/// Quantos `p` existem com o termo `i` na posição `p + offsets[i]`, para todo
/// `i`, e quantos desses `p` ficam antes de `title_span` (no título). As listas
/// estão em ordem crescente, então basta um ponteiro por termo.
pub(crate) fn count_phrase(offsets: &[u32], positions: &[Vec<u32>], at: &mut [usize], first_only: bool, title_span: u32) -> (u32, u32) {
    at.fill(0);
    let (mut count, mut in_title) = (0, 0);
    'anchors: for &p0 in &positions[0] {
        let Some(start) = p0.checked_sub(offsets[0]) else { continue };
        for i in 1..positions.len() {
            let want = u64::from(start) + u64::from(offsets[i]);
            let list = &positions[i];
            while at[i] < list.len() && u64::from(list[at[i]]) < want {
                at[i] += 1;
            }
            match list.get(at[i]) {
                // As próximas âncoras são maiores: nenhuma casa mais.
                None => break 'anchors,
                Some(&p) if u64::from(p) != want => continue 'anchors,
                Some(_) => {}
            }
        }
        count += 1;
        in_title += u32::from(start < title_span);
        if first_only {
            break;
        }
    }
    (count, in_title)
}
