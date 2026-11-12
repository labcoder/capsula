use super::*;
use async_channel::Sender;
use kumochi_protocol::message::MAX_HOLD_CHUNKS;

#[test]
fn dropping_an_owner_drains_its_committed_unforwarded_queue() {
    let gate = CollectionGate::default();
    let generation = gate.arm(GestureId(1)).unwrap();
    let owner = MicOwner::start_with(
        generation,
        Arc::new(AtomicBool::new(true)),
        |_, _, _, pcm| {
            pcm.try_send(MicPcm {
                gesture: GestureId(1),
                seq: 0,
                rate: 8_000,
                last: true,
                samples: vec![17, -93, 405],
            })
            .unwrap();
            Ok(Finalization::Released { packets: 1 })
        },
    )
    .unwrap();
    let pending = owner.pcm.clone();
    assert_eq!(
        owner.completion.recv_blocking().unwrap(),
        Ok(Finalization::Released { packets: 1 })
    );
    assert_eq!(pending.len(), 1);
    drop(owner);
    assert_eq!(pending.try_recv(), Err(async_channel::TryRecvError::Closed));
    assert_eq!(gate.arm(GestureId(2)).unwrap_err(), MicError::Stale);
}

#[test]
fn native_batch_commit_rejects_revocation_after_authorization() {
    use crate::platform::windows::microphone::copy_batch;
    use std::sync::atomic::AtomicU64;

    for cause in 0..4 {
        let at = Instant::now();
        let gate = CollectionGate::default();
        let generation = gate.arm_at(GestureId(1), at).unwrap();
        generation.advance(ARMED, OPENING).unwrap();
        generation.advance(OPENING, RUNNING).unwrap();
        generation.advance(RUNNING, COLLECTING).unwrap();
        let session = Arc::new(AtomicBool::new(true));
        let elapsed = Arc::new(AtomicU64::new(0));
        let permit = Arc::clone(&session);
        let clock = Arc::clone(&elapsed);
        let (checked, authorized) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            let copy = scope.spawn(move || {
                let mut raw = [7; 4];
                let copied = copy_batch(
                    &mut raw,
                    Some(&[17, 99, 240, 31]),
                    0,
                    || {
                        generation.mode(
                            &permit,
                            at + Duration::from_millis(clock.load(Ordering::Acquire)),
                        ) == Ok(Mode::Collecting)
                    },
                    || {
                        checked.send(()).unwrap();
                        resumed.recv_timeout(Duration::from_secs(2)).unwrap();
                    },
                );
                (copied, raw)
            });
            authorized.recv_timeout(Duration::from_secs(2)).unwrap();
            match cause {
                0 => gate.release(GestureId(1)),
                1 => gate.cancel(GestureId(1)),
                2 => session.store(false, Ordering::Release),
                _ => elapsed.store(60_000, Ordering::Release),
            }
            resume.send(()).unwrap();
            assert_eq!(copy.join().unwrap(), (0, [0; 4]));
        });
    }
    let mut raw = [0; 4];
    assert_eq!(
        copy_batch(&mut raw, Some(&[17, 99, 240, 31]), 0, || true, || {}),
        4
    );
    assert_eq!(raw, [17, 99, 240, 31]);
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "a fake worker must finish within this test's bound"
)]
fn receipt(owner: &mut MicOwner) -> Result<StopReceipt, MicError> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(receipt) = owner.try_receipt() {
            return receipt;
        }
        assert!(Instant::now() < until, "fake source must retire");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_owner_waits_for_a_delayed_fake_open_to_return_and_join() {
    let gate = CollectionGate::default();
    let generation = gate.arm(GestureId(1)).unwrap();
    let (entered, opened) = std::sync::mpsc::channel();
    let (end, ending) = std::sync::mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let finish = Arc::clone(&finished);
    let mut owner = MicOwner::start_with(
        generation,
        Arc::new(AtomicBool::new(true)),
        move |token, session, _, _| {
            entered.send(()).unwrap();
            ending.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(token.running(), Err(MicError::Stale));
            assert!(!token.collecting(session));
            finish.store(true, Ordering::Release);
            Ok(Finalization::Discarded(MicError::Cancelled))
        },
    )
    .unwrap();
    opened.recv_timeout(Duration::from_secs(2)).unwrap();
    owner.stop(StopReason::Cancel);
    assert_eq!(owner.try_receipt(), None);
    assert_eq!(gate.arm(GestureId(2)).unwrap_err(), MicError::Stale);
    end.send(()).unwrap();
    assert_eq!(
        receipt(&mut owner),
        Ok(StopReceipt {
            gesture: GestureId(1),
            finalization: Finalization::Discarded(MicError::Cancelled)
        })
    );
    assert!(finished.load(Ordering::Acquire));
    assert_eq!(gate.arm(GestureId(2)).unwrap().gesture(), GestureId(2));
}

#[test]
fn actual_owner_collects_only_after_matching_ui_acceptance_then_finalizes_once() {
    let gate = CollectionGate::default();
    let generation = gate.arm(GestureId(1)).unwrap();
    let (entered, opened) = std::sync::mpsc::channel();
    let (copied, copying) = std::sync::mpsc::channel();
    let mut owner = MicOwner::start_with(
        generation,
        Arc::new(AtomicBool::new(true)),
        move |token, session, running, output| {
            token.running()?;
            running
                .try_send(Running {
                    gesture: token.gesture(),
                    rate: 8_000,
                })
                .unwrap();
            entered.send(()).unwrap();
            while token.mode(session, Instant::now())? == Mode::Preparing {
                thread::park_timeout(Duration::from_millis(1));
            }
            let mut packets = Packetizer::new(token.gesture(), format(Sample::I16, 1))?;
            for sample in [17, -93, 405] {
                assert!(token.collecting(session));
                packets.push(sample, output)?;
            }
            copied.send(()).unwrap();
            while token.mode(session, Instant::now())? == Mode::Collecting {
                thread::park_timeout(Duration::from_millis(1));
            }
            Ok(Finalization::Released {
                packets: packets.finish(output)?,
            })
        },
    )
    .unwrap();
    opened.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        owner.pcm.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    );
    let running = owner.running.try_recv().unwrap();
    owner.accept_running(running).unwrap();
    copying.recv_timeout(Duration::from_secs(2)).unwrap();
    owner.stop(StopReason::Release);
    assert_eq!(
        receipt(&mut owner),
        Ok(StopReceipt {
            gesture: GestureId(1),
            finalization: Finalization::Released { packets: 1 }
        })
    );
    assert_eq!(
        owner.pcm.try_recv().unwrap(),
        MicPcm {
            gesture: GestureId(1),
            seq: 0,
            rate: 8_000,
            last: true,
            samples: vec![17, -93, 405]
        }
    );
    assert_eq!(owner.try_receipt(), None);
}

#[test]
fn actual_owner_observes_session_loss_without_ui_or_sample_callbacks() {
    let gate = CollectionGate::default();
    let generation = gate.arm(GestureId(1)).unwrap();
    let session = Arc::new(AtomicBool::new(true));
    let (entered, opened) = std::sync::mpsc::channel();
    let mut owner = MicOwner::start_with(
        generation,
        Arc::clone(&session),
        move |token, session, _, _| {
            entered.send(()).unwrap();
            while session.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_millis(1));
            }
            assert_eq!(
                token.mode(session, Instant::now()),
                Err(MicError::SessionLost)
            );
            Ok(Finalization::Discarded(MicError::SessionLost))
        },
    )
    .unwrap();
    opened.recv_timeout(Duration::from_secs(2)).unwrap();
    session.store(false, Ordering::Release);
    assert_eq!(
        receipt(&mut owner),
        Ok(StopReceipt {
            gesture: GestureId(1),
            finalization: Finalization::Discarded(MicError::SessionLost)
        })
    );
    assert_eq!(
        owner.pcm.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    );
}

#[test]
fn real_owner_queue_accepts_four_packets_and_fails_on_the_fifth() {
    let generation = CollectionGate::default().arm(GestureId(1)).unwrap();
    let mut owner = MicOwner::start_with(
        generation,
        Arc::new(AtomicBool::new(true)),
        |_, _, _, output| {
            for seq in 0..4 {
                output
                    .try_send(MicPcm {
                        gesture: GestureId(1),
                        seq,
                        rate: 8_000,
                        last: false,
                        samples: vec![17; 160],
                    })
                    .unwrap();
            }
            let error = output
                .try_send(MicPcm {
                    gesture: GestureId(1),
                    seq: 4,
                    rate: 8_000,
                    last: true,
                    samples: vec![19],
                })
                .unwrap_err();
            assert!(matches!(error, async_channel::TrySendError::Full(_)));
            Ok(Finalization::Discarded(MicError::Overflow))
        },
    )
    .unwrap();
    assert_eq!(
        receipt(&mut owner),
        Ok(StopReceipt {
            gesture: GestureId(1),
            finalization: Finalization::Discarded(MicError::Overflow)
        })
    );
    assert_eq!(owner.pcm.len(), 0);
}

fn prepared() -> (CollectionGate, MicOwner, Sender<MicPcm>, Instant) {
    let gate = CollectionGate::default();
    let at = Instant::now();
    let generation = gate.arm_at(GestureId(7), at).unwrap();
    generation.advance(ARMED, OPENING).unwrap();
    generation.startup.set(at).unwrap();
    let (_, running) = async_channel::bounded(1);
    let (_, completion) = async_channel::bounded(1);
    let (output, pcm) = async_channel::bounded(1);
    let owner = MicOwner {
        generation,
        session: Arc::new(AtomicBool::new(true)),
        wake: Arc::new(OnceLock::new()),
        completion,
        reported: false,
        running,
        pcm,
    };
    (gate, owner, output, at)
}

fn publish(owner: &MicOwner) {
    owner.generation.running().unwrap();
    owner
        .accept_running(Running {
            gesture: GestureId(7),
            rate: 8_000,
        })
        .unwrap();
}

#[test]
fn release_cancel_and_reset_fence_delayed_open_and_running() {
    for running in [false, true] {
        for cancel in [false, true] {
            let (gate, owner, _, at) = prepared();
            if running {
                publish(&owner);
            }
            gate.release(GestureId(7));
            if cancel {
                gate.cancel_current();
            }
            let expected = if cancel {
                Err(MicError::Cancelled)
            } else {
                Ok(Mode::Release)
            };
            assert_eq!(owner.generation.mode(&owner.session, at), expected);
            assert!(!owner.generation.collecting(&owner.session));
            assert_eq!(owner.generation.running(), Err(MicError::Stale));
            assert_eq!(
                owner.accept_running(Running {
                    gesture: GestureId(7),
                    rate: 8_000
                }),
                Err(if cancel {
                    MicError::Cancelled
                } else {
                    MicError::Stale
                })
            );
            gate.release(GestureId(7));
            assert_eq!(owner.generation.mode(&owner.session, at), expected);
            assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
        }
    }
}

#[test]
fn collection_requires_running_and_ui_acceptance() {
    let (_, owner, _, at) = prepared();
    assert_eq!(
        owner.generation.mode(&owner.session, at),
        Ok(Mode::Preparing)
    );
    assert_eq!(
        owner.accept_running(Running {
            gesture: GestureId(7),
            rate: 8_000
        }),
        Err(MicError::Stale)
    );
    owner.generation.running().unwrap();
    assert!(!owner.generation.collecting(&owner.session));
    assert_eq!(
        owner.accept_running(Running {
            gesture: GestureId(6),
            rate: 8_000
        }),
        Err(MicError::Stale)
    );
    owner
        .accept_running(Running {
            gesture: GestureId(7),
            rate: 8_000,
        })
        .unwrap();
    assert!(owner.generation.collecting(&owner.session));
    assert_eq!(
        owner.generation.advance(ARMED, OPENING),
        Err(MicError::Stale)
    );
}

#[test]
fn session_loss_and_local_cap_need_no_ui_or_sample_callbacks() {
    for running in [false, true] {
        let (_, owner, _, at) = prepared();
        if running {
            publish(&owner);
            assert_eq!(
                owner
                    .generation
                    .mode(&owner.session, at + Duration::from_millis(59_999)),
                Ok(Mode::Collecting)
            );
        }
        owner.session.store(false, Ordering::Release);
        assert_eq!(
            owner.generation.mode(&owner.session, at),
            Err(MicError::SessionLost)
        );
        owner.session.store(true, Ordering::Release);
        assert_eq!(
            owner.generation.mode(&owner.session, at),
            Err(MicError::Cancelled)
        );
        assert!(!owner.generation.collecting(&owner.session));

        let (_, owner, _, at) = prepared();
        if running {
            publish(&owner);
        }
        assert_eq!(
            owner
                .generation
                .mode(&owner.session, at + Duration::from_secs(60)),
            Err(MicError::Deadline)
        );
        assert_eq!(
            owner.generation.mode(&owner.session, at),
            Err(MicError::Cancelled)
        );
        assert!(!owner.generation.collecting(&owner.session));
    }
}

#[test]
fn startup_expires_until_ui_acceptance_and_never_releases_device_ownership() {
    for running in [false, true] {
        let (gate, owner, _, at) = prepared();
        if running {
            owner.generation.running().unwrap();
        }
        assert_eq!(
            owner.generation.mode(
                &owner.session,
                at + Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS - 1))
            ),
            Ok(Mode::Preparing)
        );
        assert_eq!(
            owner.generation.mode(
                &owner.session,
                at + Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS))
            ),
            Err(MicError::Deadline)
        );
        assert_eq!(
            owner.accept_running(Running {
                gesture: GestureId(7),
                rate: 8_000
            }),
            Err(MicError::Cancelled)
        );
        assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
    }
    let (_, owner, _, at) = prepared();
    publish(&owner);
    assert_eq!(
        owner.generation.mode(
            &owner.session,
            at + Duration::from_millis(u64::from(HOLD_START_TIMEOUT_MS))
        ),
        Ok(Mode::Collecting)
    );
}

#[test]
fn completion_and_unconfirmed_channel_close_are_consumed_once() {
    let (_, mut owner, _, _) = prepared();
    assert_eq!(owner.try_receipt(), Some(Err(MicError::Unconfirmed)));
    assert_eq!(owner.try_receipt(), None);
    let (_, mut owner, _, _) = prepared();
    let (completed, completion) = async_channel::bounded(1);
    owner.completion = completion;
    owner.stop(StopReason::Release);
    completed
        .try_send(Ok(Finalization::Released { packets: 0 }))
        .unwrap();
    drop(completed);
    assert_eq!(
        owner.try_receipt(),
        Some(Ok(StopReceipt {
            gesture: GestureId(7),
            finalization: Finalization::Released { packets: 0 }
        }))
    );
    assert_eq!(owner.try_receipt(), None);
}

#[test]
fn only_successful_matching_join_allows_a_new_generation() {
    let (gate, owner, _, _) = prepared();
    publish(&owner);
    owner.stop(StopReason::Release);
    owner.stop(StopReason::Release);
    assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
    assert_eq!(
        owner.joined(&Ok(Ok(Finalization::Released { packets: 2 }))),
        Ok(StopReceipt {
            gesture: GestureId(7),
            finalization: Finalization::Released { packets: 2 }
        })
    );
    let next = gate.arm(GestureId(8)).unwrap();
    assert_eq!(
        owner.generation.mode(&owner.session, Instant::now()),
        Err(MicError::Stale)
    );
    assert_eq!(
        next.mode(&owner.session, Instant::now()),
        Ok(Mode::Preparing)
    );
    assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);

    let (gate, owner, _, _) = prepared();
    assert_eq!(
        owner.joined(&Ok(Err(MicError::Unconfirmed))),
        Err(MicError::Unconfirmed)
    );
    assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
    assert_eq!(owner.joined(&Err(Box::new(()))), Err(MicError::Unconfirmed));
    assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
}

#[test]
fn cancel_upgrades_release_and_scrubs_reachable_queued_audio() {
    let (gate, owner, output, _) = prepared();
    publish(&owner);
    output
        .try_send(MicPcm {
            gesture: GestureId(7),
            seq: 0,
            rate: 8_000,
            last: true,
            samples: vec![1234],
        })
        .unwrap();
    owner.stop(StopReason::Release);
    owner.stop(StopReason::Cancel);
    owner.stop(StopReason::Release);
    assert_eq!(
        owner.joined(&Ok(Ok(Finalization::Released { packets: 1 }))),
        Ok(StopReceipt {
            gesture: GestureId(7),
            finalization: Finalization::Discarded(MicError::Cancelled)
        })
    );
    assert_eq!(owner.pcm.len(), 0);
    assert_eq!(gate.arm(GestureId(7)).unwrap_err(), MicError::Stale);
}

#[test]
fn reset_before_first_arm_does_not_poison_the_gate() {
    let gate = CollectionGate::default();
    gate.cancel_current();
    assert_eq!(gate.arm(GestureId(0)).unwrap_err(), MicError::Stale);
    assert_eq!(gate.arm(GestureId(1)).unwrap().gesture(), GestureId(1));
}

#[test]
fn refused_admission_can_retire_without_a_device_but_started_sources_cannot() {
    let gate = CollectionGate::default();
    let generation = gate.arm(GestureId(1)).unwrap();
    gate.cancel(GestureId(1));
    generation.discard_unstarted().unwrap();
    assert_eq!(generation.discard_unstarted(), Err(MicError::Stale));
    assert_eq!(gate.arm(GestureId(2)).unwrap().gesture(), GestureId(2));
    let (gate, owner, _, _) = prepared();
    owner.stop(StopReason::Cancel);
    assert_eq!(owner.generation.discard_unstarted(), Err(MicError::Stale));
    assert_eq!(gate.arm(GestureId(8)).unwrap_err(), MicError::Stale);
}

fn format(sample: Sample, channels: u16) -> Format {
    Format {
        rate: 8_000,
        channels,
        sample,
    }
}

#[test]
fn exact_packet_boundaries_preserve_samples_without_empty_or_duplicate_terminals() {
    for (len, sizes) in [
        (0, vec![]),
        (1, vec![1]),
        (159, vec![159]),
        (160, vec![160]),
        (161, vec![160, 1]),
    ] {
        let (output, input) = async_channel::bounded(2);
        let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
        let samples: Vec<_> = [1234, -2100, 77, -9, 8192]
            .into_iter()
            .cycle()
            .take(len)
            .collect();
        for &sample in &samples {
            packets.push(sample, &output).unwrap();
        }
        assert_eq!(
            packets.finish(&output).unwrap(),
            u32::try_from(sizes.len()).unwrap()
        );
        let mut restored = Vec::new();
        for (seq, size) in sizes.iter().enumerate() {
            let packet = input.try_recv().unwrap();
            assert_eq!(
                (
                    packet.gesture,
                    packet.seq,
                    packet.rate,
                    packet.last,
                    packet.samples.len()
                ),
                (
                    GestureId(7),
                    u32::try_from(seq).unwrap(),
                    8_000,
                    seq + 1 == sizes.len(),
                    *size
                )
            );
            assert_eq!(packet.check(), Ok(()));
            restored.extend(packet.samples);
        }
        assert_eq!(restored, samples);
        assert_eq!(input.len(), 0);
    }
}

#[test]
fn one_full_packet_is_retained_until_a_later_sample_proves_nonfinal() {
    let (output, input) = async_channel::bounded(2);
    let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
    for _ in 0..160 {
        packets.push(-2345, &output).unwrap();
    }
    assert_eq!(input.len(), 0);
    packets.push(6789, &output).unwrap();
    let first = input.try_recv().unwrap();
    assert_eq!(
        (first.seq, first.last, first.samples),
        (0, false, vec![-2345; 160])
    );
    assert_eq!(packets.finish(&output).unwrap(), 2);
    let last = input.try_recv().unwrap();
    assert_eq!((last.seq, last.last, last.samples), (1, true, vec![6789]));
}

#[test]
fn queue_overflow_and_discard_stay_failed_after_valid_samples() {
    let (output, input) = async_channel::bounded(1);
    let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
    for _ in 0..320 {
        packets.push(1234, &output).unwrap();
    }
    assert_eq!(packets.push(-4321, &output), Err(MicError::Overflow));
    assert_eq!(input.try_recv().unwrap().samples, vec![1234; 160]);
    assert_eq!(packets.push(0, &output), Err(MicError::Overflow));
    assert_eq!(packets.finish(&output), Err(MicError::Overflow));
    assert_eq!(input.len(), 0);
    let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
    packets.push(1234, &output).unwrap();
    packets.clear();
    assert_eq!(packets.push(0, &output), Err(MicError::Overflow));
    assert_eq!(packets.finish(&output), Err(MicError::Overflow));
}

#[test]
fn exact_hold_limit_is_valid_but_one_more_sample_is_terminal() {
    for extra in [false, true] {
        let (output, input) = async_channel::bounded(1);
        let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
        for chunk in 0..MAX_HOLD_CHUNKS {
            for _ in 0..160 {
                packets
                    .push(i16::try_from(chunk).unwrap(), &output)
                    .unwrap();
            }
            if chunk > 0 {
                let packet = input.try_recv().unwrap();
                assert_eq!(
                    (packet.seq, packet.last, packet.samples),
                    (
                        chunk - 1,
                        false,
                        vec![i16::try_from(chunk - 1).unwrap(); 160]
                    )
                );
            }
        }
        if extra {
            assert_eq!(packets.push(0, &output), Err(MicError::Overflow));
            assert_eq!(packets.finish(&output), Err(MicError::Overflow));
            assert_eq!(packets.push(1, &output), Err(MicError::Overflow));
        } else {
            assert_eq!(packets.finish(&output).unwrap(), 3000);
            let packet = input.try_recv().unwrap();
            assert_eq!(
                (packet.seq, packet.last, packet.samples),
                (2999, true, vec![2999; 160])
            );
        }
    }
}

#[test]
fn documented_pcm_conversions_are_finite_and_channel_order_independent() {
    assert_eq!(format(Sample::U8, 1).mono(&[0]), Ok(-32768));
    assert_eq!(format(Sample::U8, 1).mono(&[128]), Ok(0));
    assert_eq!(format(Sample::U8, 1).mono(&[255]), Ok(32512));
    assert_eq!(
        format(Sample::I16, 2).mono(&[0x00, 0x40, 0x00, 0xe0]),
        Ok(4096)
    );
    assert_eq!(format(Sample::I24, 1).mono(&[0x00, 0x00, 0x80]), Ok(-32768));
    assert_eq!(format(Sample::I24, 1).mono(&[0xff, 0xff, 0x7f]), Ok(32767));
    assert_eq!(
        format(Sample::I32, 1).mono(&i32::MIN.to_le_bytes()),
        Ok(-32768)
    );
    assert_eq!(
        format(Sample::I32, 1).mono(&i32::MAX.to_le_bytes()),
        Ok(32767)
    );
    assert_eq!(
        format(Sample::F32, 1).mono(&(-0.5_f32).to_le_bytes()),
        Ok(-16384)
    );
    assert_eq!(
        format(Sample::F32, 1).mono(&2.0_f32.to_le_bytes()),
        Ok(32767)
    );
    assert_eq!(
        format(Sample::F32, 1).mono(&f32::NAN.to_le_bytes()),
        Err(MicError::Format)
    );
    assert_eq!(
        format(Sample::F32, 1).mono(&f32::INFINITY.to_le_bytes()),
        Err(MicError::Format)
    );
    assert_eq!(format(Sample::I16, 2).mono(&[0]), Err(MicError::Format));
    for rate in [0, 7_999, 8_001, 192_001] {
        assert_eq!(
            Format {
                rate,
                ..format(Sample::I16, 1)
            }
            .check()
            .unwrap_err(),
            MicError::Format
        );
    }
    assert_eq!(
        format(Sample::I16, 3).check().unwrap_err(),
        MicError::Format
    );
    assert_eq!(
        format(Sample::I16, 0).check().unwrap_err(),
        MicError::Format
    );
    assert_eq!(
        Format {
            rate: 192_000,
            ..format(Sample::I16, 1)
        }
        .check()
        .unwrap()
        .rate,
        192_000
    );
}

#[test]
fn outer_audio_carriers_redact_text_and_byte_list_sentinels() {
    let (_, owner, output, _) = prepared();
    let marker = b"private-mic-marker";
    let samples: Vec<i16> = marker.iter().copied().map(i16::from).collect();
    let packet = MicPcm {
        gesture: GestureId(7),
        seq: 0,
        rate: 8_000,
        last: true,
        samples: samples.clone(),
    };
    let packet_debug = format!("{packet:?}");
    output.try_send(packet).unwrap();
    let mut packets = Packetizer::new(GestureId(7), format(Sample::I16, 1)).unwrap();
    for sample in &samples {
        packets.push(*sample, &output).unwrap();
    }
    let debug = format!("{owner:?} {packets:?} {packet_debug}");
    assert!(!debug.contains("private-mic-marker"));
    assert!(!debug.contains(&format!("{samples:?}")));
    assert!(!debug.contains(&format!("{marker:?}")));
}

const CLEAN: PacketStatus = PacketStatus {
    frames: 0,
    discontinuity: false,
    timestamp_error: false,
};
const GAP: PacketStatus = PacketStatus {
    discontinuity: true,
    ..CLEAN
};
const CLOCK: PacketStatus = PacketStatus {
    timestamp_error: true,
    ..CLEAN
};

fn collect(packets: &[(bool, bool, &[i16], PacketStatus)]) -> Result<Vec<i16>, MicError> {
    let mut boundary = PacketBoundary::default();
    let mut pcm = Vec::new();
    for &(live, drained, samples, flags) in packets {
        let frames = u32::try_from(samples.len()).unwrap();
        if boundary.packet(live, drained, PacketStatus { frames, ..flags })? {
            pcm.extend_from_slice(samples);
        }
    }
    Ok(pcm)
}

#[test]
fn startup_discards_the_packet_that_may_straddle_collection_acceptance() {
    let mut boundary = PacketBoundary::default();
    let mut packet =
        |live, drained, frames| boundary.packet(live, drained, PacketStatus { frames, ..CLEAN });
    assert_eq!(packet(false, false, 7), Ok(false));
    assert_eq!(packet(true, false, 7), Ok(false));
    assert_eq!(packet(true, true, 0), Ok(false));
    assert_eq!(packet(true, true, 7), Ok(false));
    assert_eq!(packet(true, true, 7), Ok(true));
    assert_eq!(packet(false, true, 7), Ok(false));
}

#[test]
fn discarded_and_first_kept_discontinuities_preserve_every_later_packet() {
    assert_eq!(
        collect(&[
            (false, false, &[91], GAP),
            (false, false, &[92], GAP),
            (true, false, &[93], GAP),
            (true, true, &[], GAP),
            (true, true, &[11], GAP),
            (true, true, &[21, 22], GAP),
            (true, true, &[31], CLEAN),
            (true, true, &[41, 42], CLEAN),
        ]),
        Ok(vec![21, 22, 31, 41, 42])
    );
}

#[test]
fn first_kept_packet_can_be_flagged_after_a_clean_or_empty_startup() {
    for empty in [false, true] {
        let mut packets: Vec<(bool, bool, &[i16], PacketStatus)> = Vec::new();
        if empty {
            packets.push((true, true, &[], CLEAN));
        }
        packets.extend_from_slice(&[
            (true, true, &[11], CLEAN),
            (true, true, &[21], GAP),
            (true, true, &[31, 32], CLEAN),
            (true, true, &[41], CLEAN),
        ]);
        assert_eq!(collect(&packets), Ok(vec![21, 31, 32, 41]));
    }
}

#[test]
fn later_kept_discontinuity_ends_the_sequence_before_clean_audio() {
    for clean in [0, 1] {
        let mut packets: Vec<(bool, bool, &[i16], PacketStatus)> =
            vec![(true, true, &[11], GAP), (true, true, &[21], CLEAN)];
        for _ in 0..clean {
            packets.push((true, true, &[31], CLEAN));
        }
        packets.extend_from_slice(&[(true, true, &[41], GAP), (true, true, &[51], CLEAN)]);
        assert_eq!(collect(&packets), Err(MicError::Discontinuity));
    }
}

#[test]
fn timestamps_remain_fatal_before_during_and_after_the_boundary() {
    for flags in [
        CLOCK,
        PacketStatus {
            discontinuity: true,
            ..CLOCK
        },
    ] {
        assert_eq!(
            collect(&[(false, false, &[], flags)]),
            Err(MicError::Timestamp)
        );
        for prefix in 0..=4 {
            let packets: Vec<_> = [
                (false, false, &[91][..], CLEAN),
                (true, false, &[92][..], CLEAN),
                (true, true, &[11][..], CLEAN),
                (true, true, &[21][..], CLEAN),
            ]
            .into_iter()
            .take(prefix)
            .chain([
                (true, true, &[31][..], flags),
                (true, true, &[41][..], CLEAN),
            ])
            .collect();
            assert_eq!(collect(&packets), Err(MicError::Timestamp));
        }
    }
}
