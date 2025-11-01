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
    order_id: u64,
    price: i64,
    qty: i64,
    side: Side,
    prev: Option<NonZeroUsize>,
    next: Option<NonZeroUsize>,
}

impl Node {
    #[inline] fn new(order_id: u64, price: i64, qty: i64, side: Side) -> Self {
        Self { order_id, price, qty, side, prev: None, next: None }
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
    fn iter_fifo<'a>(&self, orders: &'a Slab<Node>) -> LevelIter<'a> {
        LevelIter { orders, cur: self.head }
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
        } else { None }
    }
}

#[derive(Default)]
struct InstrumentBook {
    bids: BTreeMap<i64, Level>,
    asks: BTreeMap<i64, Level>,
    orders: Slab<Node>,
    // Cached best prices and quantities for O(1) BBO
    best_bid: Option<i64>,
    best_ask: Option<i64>,
    best_bid_qty: i64,
    best_ask_qty: i64,
}

impl InstrumentBook {
    #[cfg(test)]
    fn new() -> Self { Self { bids: BTreeMap::new(), asks: BTreeMap::new(), orders: Slab::with_capacity(1<<20) } }

    #[inline]
    fn with_capacity(order_slab_capacity: usize) -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            orders: Slab::with_capacity(order_slab_capacity),
            best_bid: None,
            best_ask: None,
            best_bid_qty: 0,
            best_ask_qty: 0,
        }
    }

    #[inline]
    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<i64, Level> {
        match side { Side::Bid => &mut self.bids, Side::Ask => &mut self.asks }
    }

    #[inline]
    fn add(&mut self, order_id: u64, price: i64, qty: i64, side: Side) -> Handle {
        let h = self.orders.insert(Node::new(order_id, price, qty, side));
        // Obtain previous tail without holding the level borrow across order mutations
        let prev_tail: Option<NonZeroUsize> = {
            let lvl = self.levels_mut(side).entry(price).or_default();
            lvl.tail
        };
        let h_nz = to_nz(h);
        if let Some(t) = prev_tail { self.orders[from_nz(t)].next = Some(h_nz); }
        let mut new_total_opt: Option<i64> = None;
        {
            let n = &mut self.orders[h];
            n.prev = prev_tail;
            n.next = None;
        }
        {
            let lvl = self.levels_mut(side).entry(price).or_default();
            if prev_tail.is_none() { lvl.head = Some(h_nz); }
            lvl.tail = Some(h_nz);
            lvl.count += 1;
            lvl.total_qty += qty;
            new_total_opt = Some(lvl.total_qty);
        }
        if let Some(new_total) = new_total_opt {
            match side {
                Side::Bid => {
                    if self.best_bid.map_or(true, |b| price > b) {
                        self.best_bid = Some(price);
                        self.best_bid_qty = new_total;
                    } else if self.best_bid == Some(price) {
                        self.best_bid_qty = new_total;
                    }
                }
                Side::Ask => {
                    if self.best_ask.map_or(true, |a| price < a) {
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
        if let Some(lvl) = self.levels_mut(side).get_mut(&price) { lvl.total_qty += new_qty - old_qty; new_total_opt = Some(lvl.total_qty); }
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
        if let Some(lvl) = self.levels_mut(side).get_mut(&price) {
            if prev.is_none() { lvl.head = next; }
            if next.is_none() { lvl.tail = prev; }
            lvl.count = lvl.count.saturating_sub(1);
            lvl.total_qty -= qty;
            remove_level = lvl.is_empty();
            if is_best && !remove_level { new_best_qty = Some(lvl.total_qty); }
        }
        {
            if remove_level {
                // Drop the mutable borrow to the level before removing it from the map
                self.levels_mut(side).remove(&price);
                match side {
                    Side::Bid => {
                        if is_best {
                            if let Some((p,l)) = self.bids.iter().next_back() {
                                self.best_bid = Some(*p);
                                self.best_bid_qty = l.total_qty;
                            } else {
                                self.best_bid = None;
                                self.best_bid_qty = 0;
                            }
                        }
                    }
                    Side::Ask => {
                        if is_best {
                            if let Some((p,l)) = self.asks.iter().next() {
                                self.best_ask = Some(*p);
                                self.best_ask_qty = l.total_qty;
                            } else {
                                self.best_ask = None;
                                self.best_ask_qty = 0;
                            }
                        }
                    }
                }
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
        for (p,l) in self.bids.iter().rev().take(n) { bids.push((*p, l.total_qty)); }
        for (p,l) in self.asks.iter().take(n) { asks.push((*p, l.total_qty)); }
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
    #[inline]
    fn book_mut(&mut self, instr: u32) -> &mut InstrumentBook {
        self.books.entry(instr).or_insert_with(|| InstrumentBook::with_capacity(self.default_slab_capacity))
    }

    #[inline]
    pub fn apply(&mut self, ev: &Event) {
        match *ev {
            Event::Add { order_id, instr, px, qty, side } => {
                let book = self.book_mut(instr);
                let h = book.add(order_id, px, qty, side);
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

    #[inline]
    pub fn apply_many(&mut self, events: &[Event]) {
        for e in events { self.apply(e); }
    }

    /// Optimized batch apply for a known instrument: reuses the same book when possible.
    /// Events for other instruments fall back to the single-event path.
    pub fn apply_many_for_instr(&mut self, instr: u32, events: &[Event]) {
        let consume_trades = self.consume_trades;
        for e in events {
            match *e {
                Event::Add { order_id, instr: ev_instr, px, qty, side } if ev_instr == instr => {
                    let h = { let b = self.book_mut(instr); b.add(order_id, px, qty, side) };
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
        for (instr, book) in self.books.iter() {
            let mut orders = Vec::with_capacity(book.orders.len());
            // Bids: best->worst (desc), FIFO per level
            for (price, lvl) in book.bids.iter().rev() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    orders.push(OrderExport {
                        order_id: n.order_id,
                        price: *price,
                        qty: n.qty,
                        side: Side::Bid,
                    });
                }
            }
            // Asks: best->worst (asc), FIFO per level
            for (price, lvl) in book.asks.iter() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    orders.push(OrderExport {
                        order_id: n.order_id,
                        price: *price,
                        qty: n.qty,
                        side: Side::Ask,
                    });
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
                let h = book.add(o.order_id, o.price, o.qty, o.side);
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
        let h1 = b.add(1, 100, 10, Side::Bid);
        let h2 = b.add(2, 100, 20, Side::Bid);
        let lvl = b.bids.get(&100).unwrap();
        let mut it = lvl.iter_fifo(&b.orders);
        assert_eq!(it.next(), Some(h1));
        assert_eq!(it.next(), Some(h2));
        assert_eq!(lvl.total_qty, 30);

        b.set_qty(h1, 5);
        let lvl = b.bids.get(&100).unwrap();
        assert_eq!(lvl.total_qty, 25);

        b.cancel(h2);
        let lvl = b.bids.get(&100).unwrap();
        assert_eq!(lvl.total_qty, 5);
        assert_eq!(lvl.count, 1);
    }

    #[test]
    fn remove_empty_levels() {
        let mut b = InstrumentBook::new();
        let h1 = b.add(1, 101, 10, Side::Ask);
        b.cancel(h1);
        assert!(b.asks.get(&101).is_none());
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