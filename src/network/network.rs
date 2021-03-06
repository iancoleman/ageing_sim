use std::collections::BTreeMap;
use std::fmt;
use std::mem;
use std::iter::{Iterator, Sum};
use rand::Rng;
use network::prefix::Prefix;
use network::node::Node;
use network::section::Section;
use network::churn::{NetworkEvent, SectionEvent};

/// A wrapper struct that handles merges in progress
/// When two sections merge, they need to handle a bunch
/// of churn events before they actually become a single
/// section. This remembers which sections are in the
/// process of merging and reports whether all of them are
/// ready to be combined.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingMerge {
    complete: BTreeMap<Prefix, bool>,
}

impl PendingMerge {
    /// Creates a new "pending merge" from a set of prefixes - the prefixes passed
    /// are the ones that are supposed to merge
    fn from_prefixes<I: IntoIterator<Item = Prefix>>(pfxs: I) -> Self {
        PendingMerge { complete: pfxs.into_iter().map(|pfx| (pfx, false)).collect() }
    }

    /// Mark a prefix as having completed the merge
    fn completed(&mut self, pfx: Prefix) {
        if let Some(entry) = self.complete.get_mut(&pfx) {
            *entry = true;
        }
    }

    /// Returns whether the sections are ready to be combined into one
    fn is_done(&self) -> bool {
        self.complete.iter().all(|(_, &complete)| complete)
    }

    /// Throws out the wrapper layer and returns the pure map
    fn into_map(self) -> BTreeMap<Prefix, bool> {
        self.complete
    }
}

/// The structure representing the whole network
/// It's a container for sections that simulates all the
/// churn and communication between them.
#[derive(Clone)]
pub struct Network {
    /// the number of "add" random events
    adds: u64,
    /// the number of "drop" random events
    drops: u64,
    /// the number of "rejoin" random events
    rejoins: u64,
    /// the number of relocations
    relocations: u64,
    /// the total number of churn events
    churn: u64,
    /// all the sections in the network indexed by prefixes
    nodes: BTreeMap<Prefix, Section>,
    /// the nodes that left the network and could rejoin in the future
    left_nodes: Vec<Node>,
    /// queues of events to be processed by each section
    event_queue: BTreeMap<Prefix, Vec<NetworkEvent>>,
    /// prefixes that are in the process of merging
    pending_merges: BTreeMap<Prefix, PendingMerge>,
}

impl Network {
    /// Starts a new network
    pub fn new() -> Network {
        let mut nodes = BTreeMap::new();
        nodes.insert(Prefix::empty(), Section::new(Prefix::empty()));
        Network {
            adds: 0,
            drops: 0,
            rejoins: 0,
            relocations: 0,
            churn: 0,
            nodes,
            left_nodes: Vec::new(),
            event_queue: BTreeMap::new(),
            pending_merges: BTreeMap::new(),
        }
    }

    /// Checks whether there are any events in the queues
    fn has_events(&self) -> bool {
        self.event_queue.values().any(|x| !x.is_empty())
    }

    /// Sends all events to the corresponding sections and processes the events passed
    /// back. The responses generate new events and the cycle continues until the queues are empty.
    /// Then. if any pending merges are ready, they are processed, too.
    pub fn process_events<R: Rng>(&mut self, rng: &mut R) {
        while self.has_events() {
            let queue = mem::replace(&mut self.event_queue, BTreeMap::new());
            for (prefix, events) in queue {
                let mut section_events = vec![];
                for event in events {
                    let result = self.nodes
                        .get_mut(&prefix)
                        .map(|section| section.handle_event(event))
                        .unwrap_or_else(Vec::new);
                    section_events.extend(result);
                    if let NetworkEvent::PrefixChange(pfx) = event {
                        if let Some(pending_merge) = self.pending_merges.get_mut(&pfx) {
                            pending_merge.completed(prefix);
                        }
                    }
                }
                for section_event in section_events {
                    self.process_single_event(rng, prefix, section_event);
                }
            }
        }
        let merges_to_finalise: Vec<_> = self.pending_merges
            .iter()
            .filter(|&(_, pm)| pm.is_done())
            .map(|(pfx, _)| *pfx)
            .collect();
        for pfx in merges_to_finalise {
            println!("Finalising a merge into {:?}", pfx);
            self.churn += 1; // counting merge as a single churn event
            let pending_merge = self.pending_merges.remove(&pfx).unwrap().into_map();
            let merged_section = self.merged_section(pending_merge.keys(), true);
            self.nodes.insert(merged_section.prefix(), merged_section);
        }
    }

    /// Processes a single response from a section and potentially inserts some events into its
    /// queue
    fn process_single_event<R: Rng>(&mut self, rng: &mut R, prefix: Prefix, event: SectionEvent) {
        match event {
            SectionEvent::NodeDropped(node) => {
                self.left_nodes.push(node);
            }
            SectionEvent::NeedRelocate(node) => {
                self.relocate(rng, node);
            }
            SectionEvent::RequestMerge => {
                self.merge(prefix);
            }
            SectionEvent::RequestSplit => {
                if let Some(section) = self.nodes.remove(&prefix) {
                    let ((sec0, ev0), (sec1, ev1)) = section.split();
                    let _ = self.event_queue.remove(&prefix);
                    self.event_queue
                        .entry(sec0.prefix())
                        .or_insert_with(Vec::new)
                        .extend(ev0);
                    self.event_queue
                        .entry(sec1.prefix())
                        .or_insert_with(Vec::new)
                        .extend(ev1);
                    self.nodes.insert(sec0.prefix(), sec0);
                    self.nodes.insert(sec1.prefix(), sec1);
                    self.churn += 1; // counting the split as one churn event
                }
            }
        }
    }

    /// Returns the section that would be the result of merging sections with the given prefixes.
    /// If `destructive` is true, the sections are actually removed from `self.nodes` to be
    /// combined.
    fn merged_section<'a, I: IntoIterator<Item = &'a Prefix> + Clone>(
        &mut self,
        prefixes: I,
        destructive: bool,
    ) -> Section {
        let mut sections: Vec<_> = prefixes
            .clone()
            .into_iter()
            .filter_map(|pfx| if destructive {
                let _ = self.event_queue.remove(pfx);
                self.nodes.remove(pfx)
            } else {
                self.nodes.get(pfx).cloned()
            })
            .collect();

        while sections.len() > 1 {
            sections.sort_by_key(|s| s.prefix());
            let section1 = sections.pop().unwrap();
            let section2 = sections.pop().unwrap();
            let section = section1.merge(section2);
            sections.push(section);
        }

        sections.pop().unwrap()
    }

    /// Calculates which sections will merge into a given prefix, creates a pending merge for them
    /// and prepares queues for churn events to be processed before the merge itself.
    fn merge(&mut self, prefix: Prefix) {
        let merged_pfx = prefix.shorten();
        if self.pending_merges.contains_key(&merged_pfx) {
            return;
        }
        println!("Initiating a merge into {:?}", merged_pfx);
        let prefixes: Vec<_> = self.nodes
            .keys()
            .filter(|&pfx| merged_pfx.is_ancestor(pfx))
            .cloned()
            .collect();

        let pending_merge = PendingMerge::from_prefixes(prefixes.iter().cloned());
        self.pending_merges.insert(merged_pfx, pending_merge);

        let merged_section = self.merged_section(prefixes.iter(), false);
        for pfx in prefixes {
            let events = self.calculate_merge_events(&merged_section, pfx);
            self.event_queue
                .entry(pfx)
                .or_insert_with(Vec::new)
                .extend(events);
        }
    }

    /// Creates the queue of events to be processed by a section `pfx` when it merges into
    /// `merged`.
    fn calculate_merge_events(&self, merged: &Section, pfx: Prefix) -> Vec<NetworkEvent> {
        let old_elders = self.nodes.get(&pfx).unwrap().elders();
        let new_elders = merged.elders();
        let mut events = vec![NetworkEvent::StartMerge(merged.prefix())];
        for lost_elder in &old_elders - &new_elders {
            events.push(NetworkEvent::Gone(lost_elder));
        }
        for gained_elder in &new_elders - &old_elders {
            events.push(NetworkEvent::Live(gained_elder));
        }
        events.push(NetworkEvent::PrefixChange(merged.prefix()));
        events
    }

    /// Adds a random node to the network by pushing an appropriate event to the queue
    pub fn add_random_node<R: Rng>(&mut self, rng: &mut R) {
        self.adds += 1;
        self.churn += 1;
        let node = Node::new(rng.gen());
        println!("Adding node {:?}", node);
        let prefix = self.prefix_for_node(node).unwrap();
        self.event_queue
            .entry(prefix)
            .or_insert_with(Vec::new)
            .push(NetworkEvent::Live(node));
    }

    /// Calculates the sum of weights for the dropping probability.
    /// When choosing the node to be dropped, every node is assigned a weight, so that older nodes
    /// have less chance of dropping. This helps in calculating which node should be dropped.
    fn total_drop_weight(&self) -> f64 {
        self.nodes
            .iter()
            .flat_map(|(_, s)| s.nodes().into_iter())
            .map(|n| n.drop_probability())
            .sum()
    }

    /// Returns the prefix a node should belong to.
    fn prefix_for_node(&self, node: Node) -> Option<Prefix> {
        self.nodes
            .keys()
            .find(|pfx| pfx.matches(node.name()))
            .cloned()
    }

    /// Chooses a new section for the given node, generates a new name for it,
    /// increases its age,  and sends a `Live` event to the section.
    fn relocate<R: Rng>(&mut self, rng: &mut R, mut node: Node) {
        self.relocations += 1;
        self.churn += 2; // leaving one section and joining another one
        let (node, neighbour) = {
            let src_section = self.nodes
                .keys()
                .find(|&pfx| pfx.matches(node.name()))
                .unwrap();
            let mut neighbours: Vec<_> = self.nodes
                .keys()
                .filter(|&pfx| pfx.is_neighbour(src_section))
                .collect();
            // prioritise sections with shorter prefixes and having less nodes to balance the network
            neighbours.sort_by_key(|pfx| (pfx.len(), self.nodes.get(pfx).unwrap().len()));
            let neighbour = if let Some(n) = neighbours.first() {
                n
            } else {
                src_section
            };
            let old_node = node.clone();
            node.relocate(rng, neighbour);
            println!(
                "Relocating {:?} from {:?} to {:?} as {:?}",
                old_node,
                src_section,
                neighbour,
                node
            );
            (node, neighbour)
        };
        self.event_queue
            .entry(*neighbour)
            .or_insert_with(Vec::new)
            .push(NetworkEvent::Live(node));
    }

    /// Drops a random node from the network by sending a `Lost` event to the section.
    /// The probability of a given node dropping is weighted based on its age.
    pub fn drop_random_node<R: Rng>(&mut self, rng: &mut R) {
        self.drops += 1;
        self.churn += 1;
        let total_weight = self.total_drop_weight();
        let mut drop = rng.gen::<f64>() * total_weight;
        let node_and_prefix = {
            let mut res = None;
            let nodes_iter = self.nodes.iter().flat_map(|(p, s)| {
                s.nodes().into_iter().map(move |n| (*p, n))
            });
            for (p, n) in nodes_iter {
                if n.drop_probability() > drop {
                    res = Some((p, n.name()));
                    break;
                }
                drop -= n.drop_probability();
            }
            res
        };
        node_and_prefix.map(|(prefix, name)| {
            println!("Dropping node {:?} from section {:?}", name, prefix);
            self.event_queue
                .entry(prefix)
                .or_insert_with(Vec::new)
                .push(NetworkEvent::Lost(name));
        });
    }

    /// Chooses a random node from among the ones that left the network and gets it to rejoin.
    /// The age of the rejoining node is reduced.
    pub fn rejoin_random_node<R: Rng>(&mut self, rng: &mut R) {
        self.rejoins += 1;
        self.churn += 1;
        rng.shuffle(&mut self.left_nodes);
        if let Some(mut node) = self.left_nodes.pop() {
            println!("Rejoining node {:?}", node);
            node.rejoined();
            let prefix = self.prefix_for_node(node).unwrap();
            self.event_queue
                .entry(prefix)
                .or_insert_with(Vec::new)
                .push(NetworkEvent::Live(node));
        }
    }

    pub fn age_distribution(&self) -> BTreeMap<u8, usize> {
        let mut result = BTreeMap::new();
        for (_, section) in &self.nodes {
            for node in section.nodes() {
                *result.entry(node.age()).or_insert(0) += 1;
            }
        }
        result
    }
}

impl fmt::Debug for Network {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(
            fmt,
            "Network {{\n\tadds: {}\n\tdrops: {}\n\trejoins: {}\n\trelocations: {}\n\ttotal churn: {}\n\ttotal nodes: {}\n\n{:?}\nleft_nodes: {:?}\n\n}}",
            self.adds,
            self.drops,
            self.rejoins,
            self.relocations,
            self.churn,
            usize::sum(self.nodes.values().map(|s| s.len())),
            self.nodes.values(),
            self.left_nodes
        )
    }
}
