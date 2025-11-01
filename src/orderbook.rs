// src/orderbook.rs Numan Thabit: extended with export/import
use crate::parser::{Event, Side};
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use slab::Slab;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;

type Handle = usize;
type Bbo = (Option<(i64,i64)>, Option<(i64,i64)>);
type Depth32 = SmallVec<[(i64,i64); 32]>;

#[inline(always)]
fn to_nz(h: Handle) -> NonZeroUsize { NonZeroUsize::new(h + 1).unwrap() }
#[inline(always)]
fn from_nz(nz: NonZeroUsize) -> Handle { nz.get() - 1 }

#[derive(Clone, Debug)]
struct Node {
    price: i64,
    qty: i64,
    side: Side,
    prev: Option<NonZeroUsize>,
    next: Option<NonZeroUsize>,
}

impl Node {
    #[inline] fn new(price: i64, qty: i64, side: Side) -> Self {
        Self { price, qty, side, prev: None, next: None }
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
    #[inline] fn is_empty(&self) -> bool { self.count == 0 }

    // Methods operating purely on Level are kept minimal; order-node mutation is handled in InstrumentBook

    /// Iterate handles FIFO from head to tail
    #[allow(dead_code)] // Used in export
    fn iter_fifo<'a>(&self, orders: &'a Slab<Node>) -> LevelIter<'a> {
        LevelIter { orders, cur: self.head }
    }
}

#[allow(dead_code)] // Used via iter_fifo in export
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
        } else { None }
    }
}

// Tick-addressable fixed grid for hot-path price levels, with overflow map fallback.
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
        for _ in 0..span { v.push(None); }
        Self { initialized: false, start_price: 0, tick, slots: v }
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
        if !self.initialized { return None; }
        let d = price - self.start_price;
        if d < 0 { return None; }
        if d % self.tick != 0 { return None; }
        let idx = (d / self.tick) as usize;
        if idx < self.slots.len() { Some(idx) } else { None }
    }

    #[inline]
    #[allow(dead_code)] // Used in tests
    fn get(&self, price: i64) -> Option<&Level> {
        if let Some(i) = self.price_to_idx(price) {
            self.slots[i].as_ref()
        } else { None }
    }

    #[inline]
    fn get_mut(&mut self, price: i64) -> Option<&mut Level> {
        if let Some(i) = self.price_to_idx(price) {
            // Safety: exclusive borrow of self allows mutable ref
            if self.slots[i].is_some() { self.slots[i].as_mut() } else { None }
        } else { None }
    }

    #[inline]
    fn get_mut_or_create(&mut self, price: i64) -> Option<&mut Level> {
        if !self.initialized { self.init_around(price); }
        if let Some(i) = self.price_to_idx(price) {
            if self.slots[i].is_none() { self.slots[i] = Some(Level::default()); }
            self.slots[i].as_mut()
        } else { None }
    }

    #[inline]
    fn remove(&mut self, price: i64) -> bool {
        if let Some(i) = self.price_to_idx(price) {
            if self.slots[i].as_ref().map(|l| l.is_empty()).unwrap_or(false) {
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
    fn new() -> Self { Self { bids_grid: PriceGrid::new(1, 16384), asks_grid: PriceGrid::new(1, 16384), bids_overflow: BTreeMap::new(), asks_overflow: BTreeMap::new(), orders: Slab::with_capacity(1<<20), best_bid: None, best_ask: None, best_bid_qty: 0, best_ask_qty: 0 } }

    #[inline]
    fn with_capacity(order_slab_capacity: usize) -> Self {
        Self {
            bids_grid: PriceGrid::new(1, 16384),
            asks_grid: PriceGrid::new(1, 16384),
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
        match side {
            Side::Bid => {
                if let Some(l) = self.bids_grid.get_mut_or_create(price) { return l; }
                self.bids_overflow.entry(price).or_default()
            }
            Side::Ask => {
                if let Some(l) = self.asks_grid.get_mut_or_create(price) { return l; }
                self.asks_overflow.entry(price).or_default()
            }
        }
    }

    #[inline]
    fn get_level_mut(&mut self, side: Side, price: i64) -> Option<&mut Level> {
        match side {
            Side::Bid => {
                if let Some(l) = self.bids_grid.get_mut(price) { return Some(l); }
                self.bids_overflow.get_mut(&price)
            }
            Side::Ask => {
                if let Some(l) = self.asks_grid.get_mut(price) { return Some(l); }
                self.asks_overflow.get_mut(&price)
            }
        }
    }

    #[inline]
    #[allow(dead_code)] // Used in tests
    fn get_level(&self, side: Side, price: i64) -> Option<&Level> {
        match side {
            Side::Bid => {
                if let Some(l) = self.bids_grid.get(price) { return Some(l); }
                self.bids_overflow.get(&price)
            }
            Side::Ask => {
                if let Some(l) = self.asks_grid.get(price) { return Some(l); }
                self.asks_overflow.get(&price)
            }
        }
    }

    #[inline]
    fn remove_level_if_empty(&mut self, side: Side, price: i64) -> bool {
        match side {
            Side::Bid => {
                if self.bids_grid.remove(price) { return true; }
                if let Some(l) = self.bids_overflow.get(&price) { if l.is_empty() { self.bids_overflow.remove(&price); return true; } }
                false
            }
            Side::Ask => {
                if self.asks_grid.remove(price) { return true; }
                if let Some(l) = self.asks_overflow.get(&price) { if l.is_empty() { self.asks_overflow.remove(&price); return true; } }
                false
            }
        }
    }

    #[inline]
    fn recompute_best_after_removal(&mut self, side: Side) {
        match side {
            Side::Bid => {
                let grid_cand = self.bids_grid.best_bid_candidate();
                let of_cand = self.bids_overflow.iter().next_back().map(|(p,l)| (*p, l.total_qty));
                let pick = match (grid_cand, of_cand) {
                    (Some(g), Some(o)) => if g.0 >= o.0 { Some(g) } else { Some(o) },
                    (Some(g), None) => Some(g),
                    (None, Some(o)) => Some(o),
                    (None, None) => None,
                };
                if let Some((p, q)) = pick { self.best_bid = Some(p); self.best_bid_qty = q; } else { self.best_bid = None; self.best_bid_qty = 0; }
            }
            Side::Ask => {
                let grid_cand = self.asks_grid.best_ask_candidate();
                let of_cand = self.asks_overflow.iter().next().map(|(p,l)| (*p, l.total_qty));
                let pick = match (grid_cand, of_cand) {
                    (Some(g), Some(o)) => if g.0 <= o.0 { Some(g) } else { Some(o) },
                    (Some(g), None) => Some(g),
                    (None, Some(o)) => Some(o),
                    (None, None) => None,
                };
                if let Some((p, q)) = pick { self.best_ask = Some(p); self.best_ask_qty = q; } else { self.best_ask = None; self.best_ask_qty = 0; }
            }
        }
    }

    #[inline]
    fn add(&mut self, price: i64, qty: i64, side: Side) -> Handle {
        let h = self.orders.insert(Node::new(price, qty, side));
        // Obtain previous tail without holding the level borrow across order mutations
        let prev_tail: Option<NonZeroUsize> = { let lvl = self.ensure_level_mut(side, price); lvl.tail };
        let h_nz = to_nz(h);
        if let Some(t) = prev_tail { self.orders[from_nz(t)].next = Some(h_nz); }
        {
            let n = &mut self.orders[h];
            n.prev = prev_tail;
            n.next = None;
        }
        let new_total_opt: Option<i64>;
        {
            let lvl = self.ensure_level_mut(side, price);
            if prev_tail.is_none() { lvl.head = Some(h_nz); }
            lvl.tail = Some(h_nz);
            lvl.count += 1;
            lvl.total_qty += qty;
            new_total_opt = Some(lvl.total_qty);
        }
        if let Some(new_total) = new_total_opt {
            match side {
                Side::Bid => {
                    if self.best_bid.is_none_or(|b| price > b) {
                        self.best_bid = Some(price);
                        self.best_bid_qty = new_total;
                    } else if self.best_bid == Some(price) {
                        self.best_bid_qty = new_total;
                    }
                }
                Side::Ask => {
                    if self.best_ask.is_none_or(|a| price < a) {
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
        if let Some(lvl) = self.get_level_mut(side, price) { lvl.total_qty += new_qty - old_qty; new_total_opt = Some(lvl.total_qty); }
        if let Some(new_total) = new_total_opt {
            match side {
                Side::Bid => if self.best_bid == Some(price) { self.best_bid_qty = new_total; },
                Side::Ask => if self.best_ask == Some(price) { self.best_ask_qty = new_total; },
            }
        }
    }

    #[inline]
    fn cancel(&mut self, h: Handle) {
        let (price, side, prev, next, qty) = {
            let n = &self.orders[h];
            (n.price, n.side, n.prev, n.next, n.qty)
        };
        if let Some(p) = prev { self.orders[from_nz(p)].next = next; }
        if let Some(nh) = next { self.orders[from_nz(nh)].prev = prev; }
        let mut remove_level = false;
        let is_best = match side { Side::Bid => self.best_bid == Some(price), Side::Ask => self.best_ask == Some(price) };
        let mut new_best_qty: Option<i64> = None;
        if let Some(lvl) = self.get_level_mut(side, price) {
            if prev.is_none() { lvl.head = next; }
            if next.is_none() { lvl.tail = prev; }
            lvl.count = lvl.count.saturating_sub(1);
            lvl.total_qty -= qty;
            remove_level = lvl.is_empty();
            if is_best && !remove_level { new_best_qty = Some(lvl.total_qty); }
        }
        {
            if remove_level {
                let _removed = self.remove_level_if_empty(side, price);
                if is_best { self.recompute_best_after_removal(side); }
            } else if let Some(q) = new_best_qty {
                match side {
                    Side::Bid => if is_best { self.best_bid_qty = q; },
                    Side::Ask => if is_best { self.best_ask_qty = q; },
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

    #[allow(dead_code)]
    fn top_n(&self, n: usize) -> (Depth32, Depth32) {
        let mut bids = SmallVec::<[(i64,i64); 32]>::new();
        let mut asks = SmallVec::<[(i64,i64); 32]>::new();
        // Bids: grid high->low, then overflow high->low
        for i in (0..self.bids_grid.slots.len()).rev() {
            if let Some(l) = &self.bids_grid.slots[i] { if !l.is_empty() {
                let p = self.bids_grid.start_price + (i as i64)*self.bids_grid.tick;
                bids.push((p, l.total_qty)); if bids.len() >= n { break; }
            }}
        }
        if bids.len() < n { for (p,l) in self.bids_overflow.iter().rev() { bids.push((*p, l.total_qty)); if bids.len() >= n { break; } } }
        // Asks: grid low->high, then overflow low->high
        for i in 0..self.asks_grid.slots.len() {
            if let Some(l) = &self.asks_grid.slots[i] { if !l.is_empty() {
                let p = self.asks_grid.start_price + (i as i64)*self.asks_grid.tick;
                asks.push((p, l.total_qty)); if asks.len() >= n { break; }
            }}
        }
        if asks.len() < n { for (p,l) in self.asks_overflow.iter() { asks.push((*p, l.total_qty)); if asks.len() >= n { break; } } }
        (bids, asks)
    }
}

pub struct OrderBook {
    _depth_for_reporting: usize,
    books: HashMap<u32, InstrumentBook>,
    index: HashMap<u64, (u32, Handle)>,
    last_instr: Option<u32>,
    consume_trades: bool,
    default_slab_capacity: usize,
}

impl OrderBook {
    pub fn new(depth_for_reporting: usize) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades: false,
            default_slab_capacity: 1<<20,
        }
    }

    #[allow(dead_code)]
    pub fn new_with_options(depth_for_reporting: usize, consume_trades: bool) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
            default_slab_capacity: 1<<20,
        }
    }

    #[allow(dead_code)]
    pub fn new_with_options_and_capacity(depth_for_reporting: usize, consume_trades: bool, default_slab_capacity: usize) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
            default_slab_capacity,
        }
    }

    pub fn set_consume_trades(&mut self, v: bool) {
        self.consume_trades = v;
    }

    #[inline]
    fn book_mut(&mut self, instr: u32) -> &mut InstrumentBook {
        self.books.entry(instr).or_insert_with(|| InstrumentBook::with_capacity(self.default_slab_capacity))
    }

    #[inline]
    pub fn apply(&mut self, ev: &Event) {
        match *ev {
            Event::Add { order_id, instr, px, qty, side } => {
                let book = self.book_mut(instr);
                let h = book.add(px, qty, side);
                self.index.insert(order_id, (instr, h));
                self.last_instr = Some(instr);
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
                if let Some((instr, h)) = self.index.remove(&order_id) {
                    let book = self.book_mut(instr);
                    book.cancel(h);
                    self.last_instr = Some(instr);
                }
            }
            Event::Trade { instr, qty, maker_order_id, .. } => {
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
    #[allow(dead_code)]
    pub fn apply_many_for_instr(&mut self, instr: u32, events: &[Event]) {
        let consume_trades = self.consume_trades;
        for e in events {
            match *e {
                Event::Add { order_id, instr: ev_instr, px, qty, side } if ev_instr == instr => {
                    let h = { let b = self.book_mut(instr); b.add(px, qty, side) };
                    self.index.insert(order_id, (instr, h));
                    self.last_instr = Some(instr);
                }
                Event::Mod { order_id, qty } => {
                    if let Some((mi, h)) = self.index.get(&order_id).copied() {
                        if mi == instr {
                            if qty > 0 { let b = self.book_mut(instr); b.set_qty(h, qty); }
                            else { let b = self.book_mut(instr); b.cancel(h); self.index.remove(&order_id); }
                            self.last_instr = Some(instr);
                        } else {
                            self.apply(e);
                        }
                    }
                }
                Event::Del { order_id } => {
                    if let Some((mi, h)) = self.index.remove(&order_id) {
                        if mi == instr {
                            let b = self.book_mut(instr);
                            b.cancel(h);
                            self.last_instr = Some(instr);
                        } else {
                            self.index.insert(order_id, (mi, h));
                            self.apply(e);
                        }
                    }
                }
                Event::Trade { instr: ev_instr, qty, maker_order_id, .. } if ev_instr == instr => {
                    self.last_instr = Some(instr);
                    if consume_trades {
                        if let Some(oid) = maker_order_id {
                            if let Some((mi, h)) = self.index.get(&oid).copied() {
                                if mi == instr {
                                    let new_qty = {
                                        let qty0 = { let b = self.book_mut(instr); b.orders[h].qty };
                                        (qty0 - qty).max(0)
                                    };
                                    if new_qty > 0 { let b = self.book_mut(instr); b.set_qty(h, new_qty); }
                                    else { let b = self.book_mut(instr); b.cancel(h); self.index.remove(&oid); }
                                } else {
                                    self.apply(e);
                                }
                            }
                        }
                    }
                }
                Event::Heartbeat => {}
                _ => { self.apply(e); }
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

    #[allow(dead_code)]
    pub fn top_n_of(&self, instr: u32, n: usize) -> Option<(Depth32, Depth32)> {
        self.books.get(&instr).map(|b| b.top_n(n))
    }

    pub fn order_count(&self) -> usize { self.index.len() }

    #[inline]
    pub fn instrument_for_order(&self, order_id: u64) -> Option<u32> {
        self.index.get(&order_id).map(|(instr, _)| *instr)
    }

    // ---------- Snapshot Export/Import ----------

    pub fn export(&self) -> BookExport {
        let mut instruments = Vec::with_capacity(self.books.len());
        // Build a fast reverse map from (instr, handle) -> order_id for snapshot assembly.
        // This preserves FIFO per price level while avoiding storing order_id in Node.
        let mut handle_to_id: HashMap<(u32, Handle), u64> = HashMap::with_capacity(self.index.len());
        for (oid, (ins, h)) in self.index.iter() {
            handle_to_id.insert((*ins, *h), *oid);
        }
        for (instr, book) in self.books.iter() {
            let mut orders = Vec::with_capacity(book.orders.len());
            // Bids: best->worst (desc), FIFO per level
            for i in (0..book.bids_grid.slots.len()).rev() {
                if let Some(lvl) = &book.bids_grid.slots[i] {
                    if !lvl.is_empty() {
                        let price = book.bids_grid.start_price + (i as i64)*book.bids_grid.tick;
                        for h in lvl.iter_fifo(&book.orders) {
                            let n = &book.orders[h];
                            if let Some(&oid) = handle_to_id.get(&(*instr, h)) {
                                orders.push(OrderExport { order_id: oid, price, qty: n.qty, side: Side::Bid });
                            }
                        }
                    }
                }
            }
            for (price, lvl) in book.bids_overflow.iter().rev() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    if let Some(&oid) = handle_to_id.get(&(*instr, h)) {
                        orders.push(OrderExport { order_id: oid, price: *price, qty: n.qty, side: Side::Bid });
                    }
                }
            }
            // Asks: best->worst (asc), FIFO per level
            for i in 0..book.asks_grid.slots.len() {
                if let Some(lvl) = &book.asks_grid.slots[i] {
                    if !lvl.is_empty() {
                        let price = book.asks_grid.start_price + (i as i64)*book.asks_grid.tick;
                        for h in lvl.iter_fifo(&book.orders) {
                            let n = &book.orders[h];
                            if let Some(&oid) = handle_to_id.get(&(*instr, h)) {
                                orders.push(OrderExport { order_id: oid, price, qty: n.qty, side: Side::Ask });
                            }
                        }
                    }
                }
            }
            for (price, lvl) in book.asks_overflow.iter() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    if let Some(&oid) = handle_to_id.get(&(*instr, h)) {
                        orders.push(OrderExport { order_id: oid, price: *price, qty: n.qty, side: Side::Ask });
                    }
                }
            }
            instruments.push(InstrumentExport { instr: *instr, orders });
        }
        BookExport { version: 1, instruments }
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