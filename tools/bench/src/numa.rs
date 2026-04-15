/// Pin the current thread to a specific CPU.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn pin_to_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn pin_to_cpu(_cpu: usize) {
    // CPU pinning not available on this platform
}

/// Get the list of CPUs on a given NUMA node.
#[cfg(target_os = "linux")]
pub fn cpus_for_numa_node(node: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/node/node{node}/cpulist");
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_cpu_list(content.trim()),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn cpus_for_numa_node(_node: usize) -> Vec<usize> {
    Vec::new()
}

/// Parse a CPU list like "0-7,16-23" into a Vec of CPU IDs.
fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let s: usize = start.parse().unwrap_or(0);
            let e: usize = end.parse().unwrap_or(s);
            cpus.extend(s..=e);
        } else if let Ok(n) = part.parse() {
            cpus.push(n);
        }
    }
    cpus
}

/// Resolve CPU list for the benchmark threads.
/// Priority: --cpu > --numa > all CPUs.
pub fn resolve_cpus(cpu_affinity: &Option<Vec<usize>>, numa_node: &Option<usize>, threads: usize) -> Vec<usize> {
    let cpus = if let Some(cpus) = cpu_affinity {
        cpus.clone()
    } else if let Some(node) = numa_node {
        cpus_for_numa_node(*node)
    } else {
        return (0..threads).collect(); // no pinning, just assign sequentially
    };

    if cpus.is_empty() {
        (0..threads).collect()
    } else {
        // Distribute threads across available CPUs (round-robin)
        (0..threads).map(|i| cpus[i % cpus.len()]).collect()
    }
}
