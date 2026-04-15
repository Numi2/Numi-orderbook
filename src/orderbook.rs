// src/orderbook.rs Numan Thabit: extended with export/import
use crate::parser::{Event, Side};
use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use slab::Slab;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

type Handle = usize;
type Bbo = (Option<(i64, i64)>, Option<(i64, i64)>);
type Depth32 = SmallVec<[(i64, i64); 32]>;
const BOOK_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const BOOK_HASH_PRIME: u64 = 0x00000100000001b3;

#[inline(always)]
fn to_nz(h: Handle) -> NonZeroUsize {
    NonZeroUsize::new(h + 1).unwrap()
}
#[inline(always)]
fn from_nz(nz: NonZeroUsize) -> Handle {
    nz.get() - 1
}

#[inline]
fn hash_u8(h: &mut u64, v: u8) {
    *h ^= u64::from(v);
    *h = h.wrapping_mul(BOOK_HASH_PRIME);
}

#[inline]
fn hash_u32(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        hash_u8(h, b);
    }
}

#[inline]
fn hash_u64(h: &mut u64, v: u64) {
    for b in v.to_le_bytes() {
        hash_u8(h, b);
    }
}

#[inline]
fn hash_i64(h: &mut u64, v: i64) {
    hash_u64(h, v as u64);
}

#[inline]
fn hash_side(h: &mut u64, side: Side) {
    hash_u8(
        h,
        match side {
            Side::Bid => 1,
            Side::Ask => 2,
        },
    );
}

#[derive(Clone, Debug)]
struct Node {
    price: i64,
    qty: i64,
    side: Side,
    prev: Option<NonZeroUsize>,
    next: Option<NonZeroUsize>,
}

impl Node {
    #[inline]
    fn new(price: i64, qty: i64, side: Side) -> Self {
        Self {
            price,
            qty,
            side,
            prev: None,
            next: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Level {
    head: Option<NonZeroUsize>,
    tail: Option<NonZeroUsize>,
    total_qty: i64,
    count: usize,
}

impl Level {
    #[inline]
    fn is_empty(&self) -> bool {
        self.count == 0
    }

    // Methods operating purely on Level are kept minimal; order-node mutation is handled in InstrumentBook

    /// Iterate handles FIFO from head to tail
    fn iter_fifo<'a>(&self, orders: &'a Slab<Node>) -> LevelIter<'a> {
        LevelIter {
            orders,
            cur: self.head,
        }
    }
}
struct LevelIter<'a> {
    orders: &'a Slab<Node>,
    cur: Option<NonZeroUsize>,
}
impl<'a> Iterator for LevelIter<'a> {
    type Item = Handle;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(nz) = self.cur {
            let h = from_nz(nz);
            self.cur = self.orders[h].next;
            Some(h)
        } else {
            None
        }
    }
}

// Tick-addressable fixed grid for hot-path price levels, with overflow map fallback.
#[derive(Debug)]
struct PriceGrid {
    initialized: bool,
    start_price: i64, // price at index 0
    tick: i64,
    slots: Vec<Option<Level>>, // length is power-of-two preferred but not required
}

impl PriceGrid {
    #[inline]
    fn new(tick: i64, span: usize) -> Self {
        let mut v = Vec::with_capacity(span);
        for _ in 0..span {
            v.push(None);
        }
        Self {
            initialized: false,
            start_price: 0,
            tick,
            slots: v,
        }
    }

    #[inline]
    fn init_around(&mut self, price: i64) {
        // Center the window around the given price (floor to tick), placing it in the middle.
        let half = (self.slots.len() / 2) as i64;
        let aligned = price - (price.rem_euclid(self.tick));
        self.start_price = aligned - half * self.tick;
        self.initialized = true;
    }

    #[inline]
    fn price_to_idx(&self, price: i64) -> Option<usize> {
        if !self.initialized {
            return None;
        }
        let d = price - self.start_price;
        if d < 0 {
            return None;
        }
        if d % self.tick != 0 {
            return None;
        }
        let idx = (d / self.tick) as usize;
        if idx < self.slots.len() {
            Some(idx)
        } else {
            None
        }
    }

    #[inline]
    #[cfg(test)]
    fn get(&self, price: i64) -> Option<&Level> {
        if let Some(i) = self.price_to_idx(price) {
            self.slots[i].as_ref()
        } else {
            None
        }
    }

    #[inline]
    fn get_mut(&mut self, price: i64) -> Option<&mut Level> {
        if let Some(i) = self.price_to_idx(price) {
            // Safety: exclusive borrow of self allows mutable ref
            if self.slots[i].is_some() {
                self.slots[i].as_mut()
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline]
    fn get_mut_or_create(&mut self, price: i64) -> Option<&mut Level> {
        if !self.initialized {
            self.init_around(price);
        }
        if let Some(i) = self.price_to_idx(price) {
            if self.slots[i].is_none() {
                self.slots[i] = Some(Level::default());
            }
            self.slots[i].as_mut()
        } else {
            None
        }
    }

    #[inline]
    fn remove(&mut self, price: i64) -> bool {
        if let Some(i) = self.price_to_idx(price) {
            if self.slots[i]
                .as_ref()
                .map(|l| l.is_empty())
                .unwrap_or(false)
            {
                self.slots[i] = None;
                return true;
            }
        }
        false
    }

    #[inline]
    fn best_bid_candidate(&self) -> Option<(i64, i64)> {
        // Highest price first
        for i in (0..self.slots.len()).rev() {
            if let Some(l) = &self.slots[i] {
                if !l.is_empty() {
                    let p = self.start_price + (i as i64) * self.tick;
                    return Some((p, l.total_qty));
                }
            }
        }
        None
    }

    #[inline]
    fn best_ask_candidate(&self) -> Option<(i64, i64)> {
        // Lowest price first
        for i in 0..self.slots.len() {
            if let Some(l) = &self.slots[i] {
                if !l.is_empty() {
                    let p = self.start_price + (i as i64) * self.tick;
                    return Some((p, l.total_qty));
                }
            }
        }
        None
    }
}

#[derive(Debug)]
struct InstrumentBook {
    bids_grid: PriceGrid,
    asks_grid: PriceGrid,
    bids_overflow: BTreeMap<i64, Level>,
    asks_overflow: BTreeMap<i64, Level>,
    orders: Slab<Node>,
    // Cached best prices and quantities for O(1) BBO
    best_bid: Option<i64>,
    best_ask: Option<i64>,
    best_bid_qty: i64,
    best_ask_qty: i64,
}

impl InstrumentBook {
    #[cfg(test)]
    fn new() -> Self {
        Self {
            bids_grid: PriceGrid::new(1, 16384),
            asks_grid: PriceGrid::new(1, 16384),
            bids_overflow: BTreeMap::new(),
            asks_overflow: BTreeMap::new(),
            orders: Slab::with_capacity(1 << 20),
            best_bid: None,
            best_ask: None,
            best_bid_qty: 0,
            best_ask_qty: 0,
        }
    }

    #[inline]
    fn with_params(order_slab_capacity: usize, tick: i64, span: usize) -> Self {
        Self {
            bids_grid: PriceGrid::new(tick, span),
            asks_grid: PriceGrid::new(tick, span),
            bids_overflow: BTreeMap::new(),
            asks_overflow: BTreeMap::new(),
            orders: Slab::with_capacity(order_slab_capacity),
            best_bid: None,
            best_ask: None,
            best_bid_qty: 0,
            best_ask_qty: 0,
        }
    }

    #[inline]
    fn ensure_level_mut(&mut self, side: Side, price: i64) -> &mut Level {
        let (grid, overflow) = match side {
            Side::Bid => (&mut self.bids_grid, &mut self.bids_overflow),
            Side::Ask => (&mut self.asks_grid, &mut self.asks_overflow),
        };
        Self::ensure_level_mut_in_grid(grid, overflow, price)
    }

    #[inline]
    fn ensure_level_mut_in_grid<'a>(
        grid: &'a mut PriceGrid,
        overflow: &'a mut BTreeMap<i64, Level>,
        price: i64,
    ) -> &'a mut Level {
        if grid.price_to_idx(price).is_some() {
            return grid
                .get_mut_or_create(price)
                .expect("price_to_idx succeeded but slot missing");
        }

        let tick = grid.tick;
        if tick == 0 || price.rem_euclid(tick) != 0 {
            return overflow.entry(price).or_default();
        }

        if overflow.contains_key(&price) {
            return overflow
                .get_mut(&price)
                .expect("overflow contains price checked above");
        }

        Self::recenter_grid(grid, overflow, price);

        if grid.price_to_idx(price).is_some() {
            return grid
                .get_mut_or_create(price)
                .expect("recentered grid should hold price");
        }

        overflow.entry(price).or_default()
    }

    #[inline]
    fn get_level_mut(&mut self, side: Side, price: i64) -> Option<&mut Level> {
        match side {
            Side::Bid => {
                if let Some(l) = self.bids_grid.get_mut(price) {
                    return Some(l);
                }
                self.bids_overflow.get_mut(&price)
            }
            Side::Ask => {
                if let Some(l) = self.asks_grid.get_mut(price) {
                    return Some(l);
                }
                self.asks_overflow.get_mut(&price)
            }
        }
    }

    #[inline]
    #[cfg(test)]
    fn get_level(&self, side: Side, price: i64) -> Option<&Level> {
        match side {
            Side::Bid => {
                if let Some(l) = self.bids_grid.get(price) {
                    return Some(l);
                }
                self.bids_overflow.get(&price)
            }
            Side::Ask => {
                if let Some(l) = self.asks_grid.get(price) {
                    return Some(l);
                }
                self.asks_overflow.get(&price)
            }
        }
    }

    #[inline]
    fn remove_level_if_empty(&mut self, side: Side, price: i64) -> bool {
        match side {
            Side::Bid => {
                if self.bids_grid.remove(price) {
                    return true;
                }
                if let Some(l) = self.bids_overflow.get(&price) {
                    if l.is_empty() {
                        self.bids_overflow.remove(&price);
                        return true;
                    }
                }
                false
            }
            Side::Ask => {
                if self.asks_grid.remove(price) {
                    return true;
                }
                if let Some(l) = self.asks_overflow.get(&price) {
                    if l.is_empty() {
                        self.asks_overflow.remove(&price);
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Recenters a grid around the given price and moves existing non-empty
    /// grid levels into the overflow map to preserve state.
    fn recenter_grid(grid: &mut PriceGrid, overflow: &mut BTreeMap<i64, Level>, around_price: i64) {
        let start = grid.start_price;
        let tick = grid.tick;
        for i in 0..grid.slots.len() {
            if let Some(lvl) = grid.slots[i].take() {
                if !lvl.is_empty() {
                    let p = start + (i as i64) * tick;
                    overflow.entry(p).or_insert(lvl);
                }
            }
        }
        grid.init_around(around_price);
    }

    #[inline]
    fn recompute_best_after_removal(&mut self, side: Side) {
        match side {
            Side::Bid => {
                let grid_cand = self.bids_grid.best_bid_candidate();
                let of_cand = self
                    .bids_overflow
                    .iter()
                    .next_back()
                    .map(|(p, l)| (*p, l.total_qty));
                let pick = match (grid_cand, of_cand) {
                    (Some(g), Some(o)) => {
                        if g.0 >= o.0 {
                            Some(g)
                        } else {
                            Some(o)
                        }
                    }
                    (Some(g), None) => Some(g),
                    (None, Some(o)) => Some(o),
                    (None, None) => None,
                };
                if let Some((p, q)) = pick {
                    self.best_bid = Some(p);
                    self.best_bid_qty = q;
                } else {
                    self.best_bid = None;
                    self.best_bid_qty = 0;
                }
            }
            Side::Ask => {
                let grid_cand = self.asks_grid.best_ask_candidate();
                let of_cand = self
                    .asks_overflow
                    .iter()
                    .next()
                    .map(|(p, l)| (*p, l.total_qty));
                let pick = match (grid_cand, of_cand) {
                    (Some(g), Some(o)) => {
                        if g.0 <= o.0 {
                            Some(g)
                        } else {
                            Some(o)
                        }
                    }
                    (Some(g), None) => Some(g),
                    (None, Some(o)) => Some(o),
                    (None, None) => None,
                };
                if let Some((p, q)) = pick {
                    self.best_ask = Some(p);
                    self.best_ask_qty = q;
                } else {
                    self.best_ask = None;
                    self.best_ask_qty = 0;
                }
            }
        }
    }

    #[inline]
    fn add(&mut self, price: i64, qty: i64, side: Side) -> Handle {
        let capacity_before = self.orders.capacity();
        let h = self.orders.insert(Node::new(price, qty, side));
        if self.orders.capacity() > capacity_before {
            crate::metrics::inc_orderbook_slab_grow();
        }
        // Obtain previous tail without holding the level borrow across order mutations
        let prev_tail: Option<NonZeroUsize> = {
            let lvl = self.ensure_level_mut(side, price);
            lvl.tail
        };
        let h_nz = to_nz(h);
        if let Some(t) = prev_tail {
            self.orders[from_nz(t)].next = Some(h_nz);
        }
        {
            let n = &mut self.orders[h];
            n.prev = prev_tail;
            n.next = None;
        }
        let new_total_opt: Option<i64>;
        {
            let lvl = self.ensure_level_mut(side, price);
            if prev_tail.is_none() {
                lvl.head = Some(h_nz);
            }
            lvl.tail = Some(h_nz);
            lvl.count += 1;
            lvl.total_qty += qty;
            new_total_opt = Some(lvl.total_qty);
        }
        if let Some(new_total) = new_total_opt {
            match side {
                Side::Bid => {
                    let improves_best = match self.best_bid {
                        Some(b) => price > b,
                        None => true,
                    };
                    if improves_best {
                        self.best_bid = Some(price);
                        self.best_bid_qty = new_total;
                    } else if self.best_bid == Some(price) {
                        self.best_bid_qty = new_total;
                    }
                }
                Side::Ask => {
                    let improves_best = match self.best_ask {
                        Some(a) => price < a,
                        None => true,
                    };
                    if improves_best {
                        self.best_ask = Some(price);
                        self.best_ask_qty = new_total;
                    } else if self.best_ask == Some(price) {
                        self.best_ask_qty = new_total;
                    }
                }
            }
        }
        h
    }

    #[inline]
    fn set_qty(&mut self, h: Handle, new_qty: i64) {
        let (price, side, old_qty) = {
            let n = &self.orders[h];
            (n.price, n.side, n.qty)
        };
        {
            let n = &mut self.orders[h];
            n.qty = new_qty;
        }
        let mut new_total_opt: Option<i64> = None;
        if let Some(lvl) = self.get_level_mut(side, price) {
            lvl.total_qty += new_qty - old_qty;
            new_total_opt = Some(lvl.total_qty);
        }
        if let Some(new_total) = new_total_opt {
            match side {
                Side::Bid => {
                    if self.best_bid == Some(price) {
                        self.best_bid_qty = new_total;
                    }
                }
                Side::Ask => {
                    if self.best_ask == Some(price) {
                        self.best_ask_qty = new_total;
                    }
                }
            }
        }
    }

    #[inline]
    fn cancel(&mut self, h: Handle) {
        let (price, side, prev, next, qty) = {
            let n = &self.orders[h];
            (n.price, n.side, n.prev, n.next, n.qty)
        };
        if let Some(p) = prev {
            self.orders[from_nz(p)].next = next;
        }
        if let Some(nh) = next {
            self.orders[from_nz(nh)].prev = prev;
        }
        let mut remove_level = false;
        let is_best = match side {
            Side::Bid => self.best_bid == Some(price),
            Side::Ask => self.best_ask == Some(price),
        };
        let mut new_best_qty: Option<i64> = None;
        if let Some(lvl) = self.get_level_mut(side, price) {
            if prev.is_none() {
                lvl.head = next;
            }
            if next.is_none() {
                lvl.tail = prev;
            }
            lvl.count = lvl.count.saturating_sub(1);
            lvl.total_qty -= qty;
            remove_level = lvl.is_empty();
            if is_best && !remove_level {
                new_best_qty = Some(lvl.total_qty);
            }
        }
        {
            if remove_level {
                let _removed = self.remove_level_if_empty(side, price);
                if is_best {
                    self.recompute_best_after_removal(side);
                }
            } else if let Some(q) = new_best_qty {
                match side {
                    Side::Bid => {
                        if is_best {
                            self.best_bid_qty = q;
                        }
                    }
                    Side::Ask => {
                        if is_best {
                            self.best_ask_qty = q;
                        }
                    }
                }
            }
        }
        self.orders.remove(h);
    }

    #[inline]
    fn bbo(&self) -> Bbo {
        let bid = self.best_bid.map(|p| (p, self.best_bid_qty));
        let ask = self.best_ask.map(|p| (p, self.best_ask_qty));
        (bid, ask)
    }
    fn top_n(&self, n: usize) -> (Depth32, Depth32) {
        let mut bids = SmallVec::<[(i64, i64); 32]>::new();
        let mut asks = SmallVec::<[(i64, i64); 32]>::new();

        let mut bid_levels = Vec::with_capacity(n.min(32));
        for i in (0..self.bids_grid.slots.len()).rev() {
            if let Some(l) = &self.bids_grid.slots[i] {
                if !l.is_empty() {
                    let p = self.bids_grid.start_price + (i as i64) * self.bids_grid.tick;
                    push_depth_level(&mut bid_levels, (p, l.total_qty));
                }
            }
        }
        for (p, l) in self.bids_overflow.iter().rev() {
            if !l.is_empty() {
                push_depth_level(&mut bid_levels, (*p, l.total_qty));
            }
        }
        bid_levels.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        bids.extend(bid_levels.into_iter().take(n));

        let mut ask_levels = Vec::with_capacity(n.min(32));
        for i in 0..self.asks_grid.slots.len() {
            if let Some(l) = &self.asks_grid.slots[i] {
                if !l.is_empty() {
                    let p = self.asks_grid.start_price + (i as i64) * self.asks_grid.tick;
                    push_depth_level(&mut ask_levels, (p, l.total_qty));
                }
            }
        }
        for (p, l) in self.asks_overflow.iter() {
            if !l.is_empty() {
                push_depth_level(&mut ask_levels, (*p, l.total_qty));
            }
        }
        ask_levels.sort_unstable_by_key(|(price, _)| *price);
        asks.extend(ask_levels.into_iter().take(n));

        (bids, asks)
    }

    fn validate_invariants(
        &self,
        instr: u32,
        reverse_index: &HashMap<(u32, Handle), u64>,
        visited: &mut HashSet<(u32, Handle)>,
    ) -> Result<(), String> {
        let mut seen_levels = HashSet::new();
        for i in 0..self.bids_grid.slots.len() {
            if let Some(level) = &self.bids_grid.slots[i] {
                let price = self.bids_grid.start_price + (i as i64) * self.bids_grid.tick;
                Self::validate_unique_level(instr, Side::Bid, price, &mut seen_levels)?;
                self.validate_level(instr, Side::Bid, price, level, reverse_index, visited)?;
            }
        }
        for (price, level) in &self.bids_overflow {
            Self::validate_unique_level(instr, Side::Bid, *price, &mut seen_levels)?;
            self.validate_level(instr, Side::Bid, *price, level, reverse_index, visited)?;
        }
        for i in 0..self.asks_grid.slots.len() {
            if let Some(level) = &self.asks_grid.slots[i] {
                let price = self.asks_grid.start_price + (i as i64) * self.asks_grid.tick;
                Self::validate_unique_level(instr, Side::Ask, price, &mut seen_levels)?;
                self.validate_level(instr, Side::Ask, price, level, reverse_index, visited)?;
            }
        }
        for (price, level) in &self.asks_overflow {
            Self::validate_unique_level(instr, Side::Ask, *price, &mut seen_levels)?;
            self.validate_level(instr, Side::Ask, *price, level, reverse_index, visited)?;
        }

        let visited_for_instr = visited
            .iter()
            .filter(|(seen_instr, _)| *seen_instr == instr)
            .count();
        if visited_for_instr != self.orders.len() {
            return Err(format!(
                "instr {instr}: visited order count does not match slab len: visited={} slab={}",
                visited_for_instr,
                self.orders.len(),
            ));
        }

        for (handle, node) in self.orders.iter() {
            if !visited.contains(&(instr, handle)) {
                return Err(format!(
                    "instr {instr}: slab handle {handle} at price {} side {:?} is not linked from any level",
                    node.price, node.side
                ));
            }
        }

        let expected_bid = match (
            self.bids_grid.best_bid_candidate(),
            self.bids_overflow
                .iter()
                .next_back()
                .map(|(price, level)| (*price, level.total_qty)),
        ) {
            (Some(grid), Some(overflow)) => {
                Some(if grid.0 >= overflow.0 { grid } else { overflow })
            }
            (Some(grid), None) => Some(grid),
            (None, Some(overflow)) => Some(overflow),
            (None, None) => None,
        };
        let expected_ask = match (
            self.asks_grid.best_ask_candidate(),
            self.asks_overflow
                .iter()
                .next()
                .map(|(price, level)| (*price, level.total_qty)),
        ) {
            (Some(grid), Some(overflow)) => {
                Some(if grid.0 <= overflow.0 { grid } else { overflow })
            }
            (Some(grid), None) => Some(grid),
            (None, Some(overflow)) => Some(overflow),
            (None, None) => None,
        };
        let expected = (expected_bid, expected_ask);
        if self.bbo() != expected {
            return Err(format!(
                "instr {instr}: cached bbo {:?} does not match depth {:?}",
                self.bbo(),
                expected
            ));
        }

        Ok(())
    }

    fn validate_unique_level(
        instr: u32,
        side: Side,
        price: i64,
        seen_levels: &mut HashSet<(u8, i64)>,
    ) -> Result<(), String> {
        let side_key = match side {
            Side::Bid => 0,
            Side::Ask => 1,
        };
        if !seen_levels.insert((side_key, price)) {
            return Err(format!(
                "instr {instr}: duplicate level at price {price} side {side:?}"
            ));
        }
        Ok(())
    }

    fn validate_level(
        &self,
        instr: u32,
        side: Side,
        price: i64,
        level: &Level,
        reverse_index: &HashMap<(u32, Handle), u64>,
        visited: &mut HashSet<(u32, Handle)>,
    ) -> Result<(), String> {
        if level.is_empty() {
            return Err(format!(
                "instr {instr}: empty level retained at price {price} side {side:?}"
            ));
        }

        let mut count = 0usize;
        let mut total_qty = 0i64;
        let mut prev = None;
        let mut cur = level.head;
        let mut tail = None;

        while let Some(nz) = cur {
            let handle = from_nz(nz);
            let node = self.orders.get(handle).ok_or_else(|| {
                format!("instr {instr}: level references missing handle {handle}")
            })?;

            if node.prev != prev {
                return Err(format!(
                    "instr {instr}: handle {handle} has prev {:?}, expected {:?}",
                    node.prev, prev
                ));
            }
            if node.price != price || node.side != side {
                return Err(format!(
                    "instr {instr}: handle {handle} is at price {} side {:?}, expected price {price} side {side:?}",
                    node.price, node.side
                ));
            }
            if node.qty <= 0 {
                return Err(format!(
                    "instr {instr}: handle {handle} has non-positive qty {}",
                    node.qty
                ));
            }
            if !reverse_index.contains_key(&(instr, handle)) {
                return Err(format!(
                    "instr {instr}: handle {handle} is linked but missing from order id index"
                ));
            }
            if !visited.insert((instr, handle)) {
                return Err(format!(
                    "instr {instr}: handle {handle} is linked from more than one level"
                ));
            }

            count += 1;
            total_qty += node.qty;
            if count > self.orders.len() {
                return Err(format!(
                    "instr {instr}: cycle detected at price {price} side {side:?}"
                ));
            }
            prev = Some(nz);
            tail = Some(nz);
            cur = node.next;
        }

        if count != level.count {
            return Err(format!(
                "instr {instr}: level price {price} side {side:?} count {} != walked {count}",
                level.count
            ));
        }
        if total_qty != level.total_qty {
            return Err(format!(
                "instr {instr}: level price {price} side {side:?} qty {} != walked {total_qty}",
                level.total_qty
            ));
        }
        if level.tail != tail {
            return Err(format!(
                "instr {instr}: level price {price} side {side:?} tail {:?} != walked {:?}",
                level.tail, tail
            ));
        }

        Ok(())
    }
}

fn push_depth_level(levels: &mut Vec<(i64, i64)>, level: (i64, i64)) {
    let cap_before = levels.capacity();
    levels.push(level);
    if levels.capacity() > cap_before {
        crate::metrics::inc_orderbook_depth_vec_grow();
    }
}

fn push_export_order(orders: &mut Vec<OrderExport>, order: OrderExport) {
    let cap_before = orders.capacity();
    orders.push(order);
    if orders.capacity() > cap_before {
        crate::metrics::inc_orderbook_export_vec_grow();
    }
}

fn push_export_instrument(instruments: &mut Vec<InstrumentExport>, instrument: InstrumentExport) {
    let cap_before = instruments.capacity();
    instruments.push(instrument);
    if instruments.capacity() > cap_before {
        crate::metrics::inc_orderbook_export_vec_grow();
    }
}

#[derive(Debug)]
pub struct OrderBook {
    _depth_for_reporting: usize,
    books: HashMap<u32, InstrumentBook>,
    index: HashMap<u64, (u32, Handle)>,
    last_instr: Option<u32>,
    consume_trades: bool,
    default_slab_capacity: usize,
    grid_tick: i64,
    grid_span: usize,
    instrument_ticks: HashMap<u32, i64>,
}

impl OrderBook {
    pub fn new(depth_for_reporting: usize) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades: false,
            default_slab_capacity: 1 << 20,
            grid_tick: 1,
            grid_span: 16384,
            instrument_ticks: HashMap::new(),
        }
    }
    pub fn new_with_options(depth_for_reporting: usize, consume_trades: bool) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
            default_slab_capacity: 1 << 20,
            grid_tick: 1,
            grid_span: 16384,
            instrument_ticks: HashMap::new(),
        }
    }
    pub fn new_with_options_and_capacity(
        depth_for_reporting: usize,
        consume_trades: bool,
        default_slab_capacity: usize,
    ) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
            default_slab_capacity,
            grid_tick: 1,
            grid_span: 16384,
            instrument_ticks: HashMap::new(),
        }
    }
    pub fn new_with_grid(
        depth_for_reporting: usize,
        consume_trades: bool,
        default_slab_capacity: usize,
        grid_tick: i64,
        grid_span: usize,
    ) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
            default_slab_capacity,
            grid_tick,
            grid_span,
            instrument_ticks: HashMap::new(),
        }
    }

    pub fn set_consume_trades(&mut self, v: bool) {
        self.consume_trades = v;
    }

    pub fn set_instrument_tick(&mut self, instr: u32, tick: i64) -> Result<(), String> {
        if tick <= 0 {
            return Err(format!("instrument {instr}: tick must be > 0, got {tick}"));
        }
        self.instrument_ticks.insert(instr, tick);
        Ok(())
    }

    pub fn set_instrument_ticks<I>(&mut self, ticks: I) -> Result<(), String>
    where
        I: IntoIterator<Item = (u32, i64)>,
    {
        for (instr, tick) in ticks {
            self.set_instrument_tick(instr, tick)?;
        }
        Ok(())
    }

    pub fn new_with_tick_table<I>(
        depth_for_reporting: usize,
        consume_trades: bool,
        default_slab_capacity: usize,
        default_grid_tick: i64,
        grid_span: usize,
        ticks: I,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = (u32, i64)>,
    {
        if default_grid_tick <= 0 {
            return Err(format!(
                "default grid tick must be > 0, got {default_grid_tick}"
            ));
        }
        if grid_span == 0 {
            return Err("grid span must be > 0".to_string());
        }
        let mut book = Self::new_with_grid(
            depth_for_reporting,
            consume_trades,
            default_slab_capacity,
            default_grid_tick,
            grid_span,
        );
        book.set_instrument_ticks(ticks)?;
        Ok(book)
    }

    #[inline]
    fn book_mut(&mut self, instr: u32) -> &mut InstrumentBook {
        let tick = self
            .instrument_ticks
            .get(&instr)
            .copied()
            .unwrap_or(self.grid_tick);
        let span = self.grid_span;
        let cap = self.default_slab_capacity;
        self.books
            .entry(instr)
            .or_insert_with(|| InstrumentBook::with_params(cap, tick, span))
    }

    #[inline]
    fn remove_order_by_id(&mut self, order_id: u64) -> Option<u32> {
        if let Some((instr, h)) = self.index.remove(&order_id) {
            let book = self.book_mut(instr);
            book.cancel(h);
            self.last_instr = Some(instr);
            Some(instr)
        } else {
            None
        }
    }

    #[inline]
    fn add_order(&mut self, order_id: u64, instr: u32, px: i64, qty: i64, side: Side) {
        // Venue reconnects, replay, or synthetic tests can deliver a repeated
        // order id. Replace atomically from the public book's point of view so
        // the old node does not remain live but unreachable from `index`.
        self.remove_order_by_id(order_id);
        let book = self.book_mut(instr);
        let h = book.add(px, qty, side);
        self.index.insert(order_id, (instr, h));
        self.last_instr = Some(instr);
    }

    #[inline]
    pub fn apply(&mut self, ev: &Event) {
        match *ev {
            Event::Add {
                order_id,
                instr,
                px,
                qty,
                side,
            } => {
                self.add_order(order_id, instr, px, qty, side);
            }
            Event::Mod { order_id, qty } => {
                if let Some((instr, h)) = self.index.get(&order_id).copied() {
                    let book = self.book_mut(instr);
                    if qty > 0 {
                        book.set_qty(h, qty);
                    } else {
                        book.cancel(h);
                        self.index.remove(&order_id);
                    }
                    self.last_instr = Some(instr);
                }
            }
            Event::Del { order_id } => {
                self.remove_order_by_id(order_id);
            }
            Event::Trade {
                instr,
                qty,
                maker_order_id,
                ..
            } => {
                self.last_instr = Some(instr);
                if self.consume_trades {
                    if let Some(oid) = maker_order_id {
                        if let Some((mi, h)) = self.index.get(&oid).copied() {
                            let book = self.book_mut(mi);
                            let new_qty = {
                                let n = &book.orders[h];
                                (n.qty - qty).max(0)
                            };
                            if new_qty > 0 {
                                book.set_qty(h, new_qty);
                            } else {
                                book.cancel(h);
                                self.index.remove(&oid);
                            }
                        }
                    }
                }
            }
            Event::Heartbeat => {}
        }
    }

    /// Optimized batch apply for a known instrument: reuses the same book when possible.
    /// Events for other instruments fall back to the single-event path.
    pub fn apply_many_for_instr(&mut self, instr: u32, events: &[Event]) {
        let consume_trades = self.consume_trades;
        for e in events {
            match *e {
                Event::Add {
                    order_id,
                    instr: ev_instr,
                    px,
                    qty,
                    side,
                } if ev_instr == instr => {
                    self.add_order(order_id, instr, px, qty, side);
                }
                Event::Mod { order_id, qty } => {
                    if let Some((mi, h)) = self.index.get(&order_id).copied() {
                        if mi == instr {
                            if qty > 0 {
                                let b = self.book_mut(instr);
                                b.set_qty(h, qty);
                            } else {
                                let b = self.book_mut(instr);
                                b.cancel(h);
                                self.index.remove(&order_id);
                            }
                            self.last_instr = Some(instr);
                        } else {
                            self.apply(e);
                        }
                    }
                }
                Event::Del { order_id } => {
                    if self
                        .index
                        .get(&order_id)
                        .map(|(mi, _)| *mi == instr)
                        .unwrap_or(false)
                    {
                        self.remove_order_by_id(order_id);
                    } else {
                        self.apply(e);
                    }
                }
                Event::Trade {
                    instr: ev_instr,
                    qty,
                    maker_order_id,
                    ..
                } if ev_instr == instr => {
                    self.last_instr = Some(instr);
                    if consume_trades {
                        if let Some(oid) = maker_order_id {
                            if let Some((mi, h)) = self.index.get(&oid).copied() {
                                if mi == instr {
                                    let new_qty = {
                                        let qty0 = {
                                            let b = self.book_mut(instr);
                                            b.orders[h].qty
                                        };
                                        (qty0 - qty).max(0)
                                    };
                                    if new_qty > 0 {
                                        let b = self.book_mut(instr);
                                        b.set_qty(h, new_qty);
                                    } else {
                                        let b = self.book_mut(instr);
                                        b.cancel(h);
                                        self.index.remove(&oid);
                                    }
                                } else {
                                    self.apply(e);
                                }
                            }
                        }
                    }
                }
                Event::Heartbeat => {}
                _ => {
                    self.apply(e);
                }
            }
        }
    }

    pub fn bbo(&self) -> Bbo {
        if let Some(instr) = self.last_instr {
            if let Some(b) = self.books.get(&instr) {
                return b.bbo();
            }
        }
        (None, None)
    }
    pub fn top_n_of(&self, instr: u32, n: usize) -> Option<(Depth32, Depth32)> {
        self.books.get(&instr).map(|b| b.top_n(n))
    }

    pub fn order_count(&self) -> usize {
        self.index.len()
    }

    #[inline]
    pub fn instrument_for_order(&self, order_id: u64) -> Option<u32> {
        self.index.get(&order_id).map(|(instr, _)| *instr)
    }
    pub fn validate_invariants(&self) -> Result<(), String> {
        let mut reverse_index: HashMap<(u32, Handle), u64> =
            HashMap::with_capacity(self.index.len());
        for (order_id, (instr, handle)) in &self.index {
            if reverse_index.insert((*instr, *handle), *order_id).is_some() {
                return Err(format!(
                    "duplicate index mapping for instr {instr} handle {handle}"
                ));
            }
            let book = self
                .books
                .get(instr)
                .ok_or_else(|| format!("order {order_id}: missing instrument book {instr}"))?;
            let node = book.orders.get(*handle).ok_or_else(|| {
                format!("order {order_id}: missing slab handle {handle} for instr {instr}")
            })?;
            if node.qty <= 0 {
                return Err(format!("order {order_id}: non-positive qty {}", node.qty));
            }
        }

        let mut visited = HashSet::with_capacity(self.index.len());
        for (instr, book) in &self.books {
            book.validate_invariants(*instr, &reverse_index, &mut visited)?;
        }

        if visited.len() != self.index.len() {
            return Err(format!(
                "visited handle count {} != index count {}",
                visited.len(),
                self.index.len()
            ));
        }

        Ok(())
    }

    // ---------- Snapshot Export/Import ----------

    pub fn export(&self) -> BookExport {
        let mut instruments = Vec::with_capacity(self.books.len());
        // Build a fast reverse map from (instr, handle) -> order_id for snapshot assembly.
        // This preserves FIFO per price level while avoiding storing order_id in Node.
        let mut handle_to_id: HashMap<(u32, Handle), u64> =
            HashMap::with_capacity(self.index.len());
        for (oid, (ins, h)) in self.index.iter() {
            handle_to_id.insert((*ins, *h), *oid);
        }
        let mut instrs: Vec<u32> = self.books.keys().copied().collect();
        instrs.sort_unstable();
        for instr in instrs {
            let Some(book) = self.books.get(&instr) else {
                continue;
            };
            let mut orders = Vec::with_capacity(book.orders.len());
            // Bids: best->worst (desc), FIFO per level
            for i in (0..book.bids_grid.slots.len()).rev() {
                if let Some(lvl) = &book.bids_grid.slots[i] {
                    if !lvl.is_empty() {
                        let price = book.bids_grid.start_price + (i as i64) * book.bids_grid.tick;
                        for h in lvl.iter_fifo(&book.orders) {
                            let n = &book.orders[h];
                            if let Some(&oid) = handle_to_id.get(&(instr, h)) {
                                push_export_order(
                                    &mut orders,
                                    OrderExport {
                                        order_id: oid,
                                        price,
                                        qty: n.qty,
                                        side: Side::Bid,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            for (price, lvl) in book.bids_overflow.iter().rev() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    if let Some(&oid) = handle_to_id.get(&(instr, h)) {
                        push_export_order(
                            &mut orders,
                            OrderExport {
                                order_id: oid,
                                price: *price,
                                qty: n.qty,
                                side: Side::Bid,
                            },
                        );
                    }
                }
            }
            // Asks: best->worst (asc), FIFO per level
            for i in 0..book.asks_grid.slots.len() {
                if let Some(lvl) = &book.asks_grid.slots[i] {
                    if !lvl.is_empty() {
                        let price = book.asks_grid.start_price + (i as i64) * book.asks_grid.tick;
                        for h in lvl.iter_fifo(&book.orders) {
                            let n = &book.orders[h];
                            if let Some(&oid) = handle_to_id.get(&(instr, h)) {
                                push_export_order(
                                    &mut orders,
                                    OrderExport {
                                        order_id: oid,
                                        price,
                                        qty: n.qty,
                                        side: Side::Ask,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            for (price, lvl) in book.asks_overflow.iter() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    if let Some(&oid) = handle_to_id.get(&(instr, h)) {
                        push_export_order(
                            &mut orders,
                            OrderExport {
                                order_id: oid,
                                price: *price,
                                qty: n.qty,
                                side: Side::Ask,
                            },
                        );
                    }
                }
            }
            push_export_instrument(&mut instruments, InstrumentExport { instr, orders });
        }
        BookExport {
            version: 1,
            instruments,
        }
    }

    pub fn from_export(exp: BookExport) -> Self {
        let mut ob = OrderBook::new(10);
        for ie in exp.instruments {
            for o in ie.orders {
                let book = ob.book_mut(ie.instr);
                let h = book.add(o.price, o.qty, o.side);
                ob.index.insert(o.order_id, (ie.instr, h));
            }
            ob.last_instr = Some(ie.instr);
        }
        ob
    }
    pub fn state_hash(&self) -> u64 {
        let mut h = BOOK_HASH_OFFSET;
        let export = self.export();
        hash_u32(&mut h, export.version);
        hash_u64(&mut h, export.instruments.len() as u64);
        for instrument in export.instruments {
            hash_u32(&mut h, instrument.instr);
            hash_u64(&mut h, instrument.orders.len() as u64);
            for order in instrument.orders {
                hash_u64(&mut h, order.order_id);
                hash_i64(&mut h, order.price);
                hash_i64(&mut h, order.qty);
                hash_side(&mut h, order.side);
            }
        }
        h
    }

    /// Export aggregated depth snapshots (top N) per instrument (not per-order).
    pub fn export_depth(&self, depth: usize) -> DepthSnapshotExport {
        let mut instruments = Vec::with_capacity(self.books.len());
        let mut instrs: Vec<u32> = self.books.keys().copied().collect();
        instrs.sort_unstable();
        for instr in instrs {
            let Some(book) = self.books.get(&instr) else {
                continue;
            };
            let (bids, asks) = book.top_n(depth);
            instruments.push(InstrumentDepthExport {
                instr,
                bids: bids.into_iter().collect(),
                asks: asks.into_iter().collect(),
            });
        }
        DepthSnapshotExport {
            version: 1,
            instruments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_within_level_and_totals() {
        let mut b = InstrumentBook::new();
        let h1 = b.add(100, 10, Side::Bid);
        let h2 = b.add(100, 20, Side::Bid);
        let lvl = b.get_level(Side::Bid, 100).unwrap();
        let mut it = lvl.iter_fifo(&b.orders);
        assert_eq!(it.next(), Some(h1));
        assert_eq!(it.next(), Some(h2));
        assert_eq!(lvl.total_qty, 30);

        b.set_qty(h1, 5);
        let lvl = b.get_level(Side::Bid, 100).unwrap();
        assert_eq!(lvl.total_qty, 25);

        b.cancel(h2);
        let lvl = b.get_level(Side::Bid, 100).unwrap();
        assert_eq!(lvl.total_qty, 5);
        assert_eq!(lvl.count, 1);
    }

    #[test]
    fn remove_empty_levels() {
        let mut b = InstrumentBook::new();
        let h1 = b.add(101, 10, Side::Ask);
        b.cancel(h1);
        assert!(b.get_level(Side::Ask, 101).is_none());
    }

    #[test]
    fn duplicate_add_replaces_existing_order() {
        let mut ob = OrderBook::new(10);
        ob.apply(&Event::Add {
            order_id: 10,
            instr: 7,
            px: 100,
            qty: 11,
            side: Side::Bid,
        });
        ob.apply(&Event::Add {
            order_id: 10,
            instr: 7,
            px: 101,
            qty: 22,
            side: Side::Bid,
        });

        assert_eq!(ob.order_count(), 1);
        let (bids, asks) = ob.top_n_of(7, 10).unwrap();
        assert_eq!(bids.as_slice(), &[(101, 22)]);
        assert!(asks.is_empty());
        assert_eq!(ob.bbo(), (Some((101, 22)), None));
        ob.validate_invariants().unwrap();
    }

    #[test]
    fn duplicate_add_can_move_between_instruments() {
        let mut ob = OrderBook::new(10);
        ob.apply(&Event::Add {
            order_id: 10,
            instr: 1,
            px: 100,
            qty: 11,
            side: Side::Bid,
        });
        ob.apply(&Event::Add {
            order_id: 10,
            instr: 2,
            px: 200,
            qty: 33,
            side: Side::Ask,
        });

        assert_eq!(ob.order_count(), 1);
        let (old_bids, old_asks) = ob.top_n_of(1, 10).unwrap();
        assert!(old_bids.is_empty());
        assert!(old_asks.is_empty());

        let (new_bids, new_asks) = ob.top_n_of(2, 10).unwrap();
        assert!(new_bids.is_empty());
        assert_eq!(new_asks.as_slice(), &[(200, 33)]);
        assert_eq!(ob.instrument_for_order(10), Some(2));
        ob.validate_invariants().unwrap();
    }

    #[test]
    fn batched_duplicate_add_replaces_existing_order() {
        let mut ob = OrderBook::new(10);
        let events = [
            Event::Add {
                order_id: 10,
                instr: 7,
                px: 100,
                qty: 11,
                side: Side::Bid,
            },
            Event::Add {
                order_id: 10,
                instr: 7,
                px: 101,
                qty: 22,
                side: Side::Bid,
            },
        ];
        ob.apply_many_for_instr(7, &events);

        assert_eq!(ob.order_count(), 1);
        let (bids, asks) = ob.top_n_of(7, 10).unwrap();
        assert_eq!(bids.as_slice(), &[(101, 22)]);
        assert!(asks.is_empty());
        ob.validate_invariants().unwrap();
    }

    #[test]
    fn top_n_sorts_grid_and_overflow_together() {
        let mut ob = OrderBook::new_with_grid(10, false, 64, 1, 4);
        ob.apply(&Event::Add {
            order_id: 1,
            instr: 9,
            px: 1000,
            qty: 10,
            side: Side::Bid,
        });
        ob.apply(&Event::Add {
            order_id: 2,
            instr: 9,
            px: 0,
            qty: 20,
            side: Side::Bid,
        });
        ob.apply(&Event::Add {
            order_id: 3,
            instr: 9,
            px: 1000,
            qty: 5,
            side: Side::Bid,
        });

        let (bids, asks) = ob.top_n_of(9, 10).unwrap();
        assert_eq!(bids.as_slice(), &[(1000, 15), (0, 20)]);
        assert!(asks.is_empty());
        assert_eq!(ob.bbo(), (Some((1000, 15)), None));
        ob.validate_invariants().unwrap();
    }

    #[test]
    fn deterministic_event_sequence_preserves_invariants_and_snapshot() {
        let mut ob = OrderBook::new_with_options(10, true);
        let events = [
            Event::Add {
                order_id: 1,
                instr: 42,
                px: 100,
                qty: 10,
                side: Side::Bid,
            },
            Event::Add {
                order_id: 2,
                instr: 42,
                px: 100,
                qty: 20,
                side: Side::Bid,
            },
            Event::Add {
                order_id: 3,
                instr: 42,
                px: 101,
                qty: 30,
                side: Side::Ask,
            },
            Event::Mod {
                order_id: 2,
                qty: 15,
            },
            Event::Trade {
                instr: 42,
                px: 100,
                qty: 4,
                maker_order_id: Some(1),
                taker_side: Some(Side::Ask),
            },
            Event::Del { order_id: 3 },
            Event::Add {
                order_id: 2,
                instr: 42,
                px: 99,
                qty: 12,
                side: Side::Bid,
            },
        ];

        for event in &events {
            ob.apply(event);
            ob.validate_invariants().unwrap();
        }

        assert_eq!(ob.order_count(), 2);
        let (bids, asks) = ob.top_n_of(42, 10).unwrap();
        assert_eq!(bids.as_slice(), &[(100, 6), (99, 12)]);
        assert!(asks.is_empty());
        assert_eq!(ob.bbo(), (Some((100, 6)), None));

        let restored = OrderBook::from_export(ob.export());
        restored.validate_invariants().unwrap();
        assert_eq!(restored.order_count(), ob.order_count());
        assert_eq!(restored.top_n_of(42, 10), ob.top_n_of(42, 10));
        assert_eq!(restored.state_hash(), ob.state_hash());
    }

    #[test]
    fn export_and_hash_are_stable_across_instrument_insertion_order() {
        let mut a = OrderBook::new(10);
        let mut b = OrderBook::new(10);

        let events_a = [
            Event::Add {
                order_id: 1,
                instr: 2,
                px: 200,
                qty: 20,
                side: Side::Ask,
            },
            Event::Add {
                order_id: 2,
                instr: 1,
                px: 100,
                qty: 10,
                side: Side::Bid,
            },
        ];
        let events_b = [events_a[1].clone(), events_a[0].clone()];

        for event in &events_a {
            a.apply(event);
        }
        for event in &events_b {
            b.apply(event);
        }

        let export_a = a.export();
        let export_b = b.export();
        assert_eq!(
            export_a
                .instruments
                .iter()
                .map(|instrument| instrument.instr)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            export_b
                .instruments
                .iter()
                .map(|instrument| instrument.instr)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(a.state_hash(), b.state_hash());
        assert_eq!(
            OrderBook::from_export(export_a).state_hash(),
            a.state_hash()
        );
        assert_eq!(
            OrderBook::from_export(export_b).state_hash(),
            b.state_hash()
        );
    }

    #[test]
    fn instrument_tick_table_configures_new_books() {
        let mut ob = OrderBook::new_with_tick_table(10, false, 64, 1, 8, [(9, 5)])
            .expect("valid tick table");
        ob.apply(&Event::Add {
            order_id: 1,
            instr: 9,
            px: 1000,
            qty: 10,
            side: Side::Bid,
        });
        ob.apply(&Event::Add {
            order_id: 2,
            instr: 10,
            px: 1000,
            qty: 10,
            side: Side::Bid,
        });

        assert_eq!(ob.books.get(&9).unwrap().bids_grid.tick, 5);
        assert_eq!(ob.books.get(&10).unwrap().bids_grid.tick, 1);
        ob.validate_invariants().unwrap();
    }

    #[test]
    fn instrument_tick_table_rejects_non_positive_ticks() {
        assert!(OrderBook::new_with_tick_table(10, false, 64, 1, 8, [(9, 0)]).is_err());
        assert!(OrderBook::new_with_tick_table(10, false, 64, 1, 0, [(9, 1)]).is_err());
        let mut ob = OrderBook::new(10);
        assert!(ob.set_instrument_tick(9, -1).is_err());
    }
}

/// Serializable snapshot format (coarse-grained; not in hot path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookExport {
    pub version: u32,
    pub instruments: Vec<InstrumentExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentExport {
    pub instr: u32,
    pub orders: Vec<OrderExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderExport {
    pub order_id: u64,
    pub price: i64,
    pub qty: i64,
    pub side: Side,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthSnapshotExport {
    pub version: u32,
    pub instruments: Vec<InstrumentDepthExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentDepthExport {
    pub instr: u32,
    pub bids: Vec<(i64, i64)>,
    pub asks: Vec<(i64, i64)>,
}
