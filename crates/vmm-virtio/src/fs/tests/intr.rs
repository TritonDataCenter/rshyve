// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which queue a reply interrupts.
//!
//! virtio-fs has a hiprio ring and a request ring, and MSI-X gives each
//! its own vector. A guest handler reads only the ring its vector names,
//! so a reply signalled under the other index reaches nobody and the
//! request never finishes. A test that only counts interrupts cannot see
//! a wrong index, so these tests read the index.

use super::*;

/// Take and clear the queue indexes the gate admitted.
fn drain(seen: &Mutex<Vec<(IntrSession, u16)>>) -> Vec<u16> {
    let mut seen = seen.lock().expect("record lock");
    let indexes = seen.iter().map(|&(_, q)| q).collect();
    seen.clear();
    indexes
}

#[test]
fn a_reply_raises_on_the_queue_it_belongs_to() {
    let (_physmap, queues, fs) = ring(8, 0);
    let seen = Arc::new(Mutex::new(Vec::new()));
    fs.interrupt.install(BackendIntr::recording(
        Arc::new(IntrGate::new()),
        Arc::clone(&seen),
    ));

    for queue_idx in [FS_HIPRIO_QUEUE, FS_REQUEST_QUEUE] {
        let session = fs
            .access
            .enter_current(queue_idx)
            .expect("a generation is open");
        let completion = fs.get_completion(
            &session,
            queue_idx,
            &queues[usize::from(queue_idx)],
        );
        drop(session);

        completion.signal();
        assert_eq!(
            drain(&seen),
            vec![queue_idx],
            "a reply on queue {queue_idx} interrupted another queue"
        );
    }
}
