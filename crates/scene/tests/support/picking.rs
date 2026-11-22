use super::frame_fixture::{handle, mapping, pointer, state};
use super::*;
use kumochi_scene::{
    Handle,
    three::pick::{DisplayedFrame, Mapping, Pick, PickError, pick},
};

fn display(
    solids: &[Solid<'_>],
    cam: Camera,
    mapping: Mapping,
    quality: Quality,
    parts: Vec<Option<Handle>>,
) -> DisplayedFrame {
    let count = solids.iter().map(|s| s.triangles.len()).sum();
    let mut out = build(solids, &cam, mapping.texels(), count);
    let (_, ids) = image(&mut out, mapping.texels(), quality);
    let mut state = state(mapping, parts);
    state.camera = cam;
    state.pose = solids.iter().map(|s| s.pose).collect();
    DisplayedFrame::new(state, ids).expect("fixture frame")
}

fn box_vertices() -> [Vec3; 8] {
    std::array::from_fn(|i| {
        Vec3::new(
            if i & 1 == 0 { -0.5 } else { 0.5 },
            if i & 2 == 0 { -0.5 } else { 0.5 },
            if i & 4 == 0 { -0.5 } else { 0.5 },
        )
    })
}

#[test]
fn scaled_and_half_resolution_mapping_has_independent_axes_and_half_open_bounds() {
    for size in [[63, 39], [31, 19]] {
        let map = mapping(size);
        let mut ids = vec![0; (size[0] * size[1]) as usize];
        ids[0] = 1;
        ids[(size[0] + 2) as usize] = 2;
        ids[(size[0] * size[1] - 1) as usize] = 1;
        let frame = DisplayedFrame::new(state(map, vec![Some(handle(0)), None]), ids)
            .expect("fixture frame");
        assert_eq!(map.dpi(), 1.5);
        for (at, expected) in [
            ([0.0, 0.0], Pick::Part(handle(0))),
            ([2.75, 1.25], Pick::Blocked),
            ([3.25, 1.25], Pick::Miss),
            (
                [f64::from(size[0]) - 0.01, f64::from(size[1]) - 0.01],
                Pick::Part(handle(0)),
            ),
        ] {
            assert_eq!(
                pick(&frame, pointer(map, at)),
                expected,
                "size {size:?}, {at:?}"
            );
        }
        for at in [
            [10.0_f64.next_down(), -20.0],
            [10.0, (-20.0_f64).next_down()],
            [241.0, -20.0],
            [10.0, 97.0],
            [f64::NAN, 0.0],
            [0.0, f64::NAN],
            [f64::INFINITY, 0.0],
            [f64::NEG_INFINITY, 0.0],
            [0.0, f64::INFINITY],
            [0.0, f64::NEG_INFINITY],
        ] {
            assert_eq!(pick(&frame, at), Pick::Miss, "pointer {at:?}");
        }
        assert_eq!(
            pick(&frame, [241.0_f64.next_down(), 97.0_f64.next_down()]),
            Pick::Part(handle(0))
        );
    }
    // 0.3 + 0.7 rounds to 1.0, so the last pointer before that end scales to exactly 2 texels.
    let edge = 1.0_f64.next_down();
    let map = Mapping::new([0.3; 2], [0.7; 2], 1.0, [2, 2]).expect("fixture mapping");
    let frame = DisplayedFrame::new(state(map, vec![Some(handle(0)), None]), vec![0, 1, 2, 0])
        .expect("fixture frame");
    assert_eq!(pick(&frame, [edge, 0.3]), Pick::Part(handle(0)));
    assert_eq!(pick(&frame, [0.3, edge]), Pick::Blocked);
    assert_eq!(pick(&frame, [edge, edge]), Pick::Miss);
}

#[test]
fn mapping_and_frame_bounds_refuse_inconsistent_snapshots() {
    for size in [[1, 4096], [4096, 1]] {
        assert_eq!(mapping(size).texels(), size);
    }
    for size in [[0, 1], [1, 0], [4097, 1], [1, 4097]] {
        assert_eq!(
            Mapping::new([0.0; 2], [1.0; 2], 1.0, size),
            Err(PickError::Size)
        );
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for axis in 0..2 {
            let mut origin = [0.0; 2];
            origin[axis] = bad;
            assert_eq!(
                Mapping::new(origin, [1.0; 2], 1.0, [1; 2]),
                Err(PickError::Size)
            );
            let mut extent = [1.0; 2];
            extent[axis] = bad;
            assert_eq!(
                Mapping::new([0.0; 2], extent, 1.0, [1; 2]),
                Err(PickError::Size)
            );
        }
    }
    for dpi in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            Mapping::new([0.0; 2], [1.0; 2], dpi, [1; 2]),
            Err(PickError::Size)
        );
    }
    for extent in [[0.0, 1.0], [1.0, 0.0], [-1.0, 1.0], [1.0, -1.0]] {
        assert_eq!(
            Mapping::new([0.0; 2], extent, 1.0, [1; 2]),
            Err(PickError::Size)
        );
    }
    for (origin, extent) in [([f64::MAX; 2], [f64::MAX; 2]), ([1e20; 2], [1.0; 2])] {
        assert_eq!(
            Mapping::new(origin, extent, 1.0, [1; 2]),
            Err(PickError::Size)
        );
    }
    let map = mapping([1; 2]);
    for ids in [vec![], vec![0, 0]] {
        assert_eq!(
            DisplayedFrame::new(state(map, vec![]), ids).err(),
            Some(PickError::Size)
        );
    }
    assert_eq!(
        DisplayedFrame::new(state(map, vec![]), vec![1]).err(),
        Some(PickError::Invalid)
    );
    let maximum = DisplayedFrame::new(state(map, vec![None; 160]), vec![160]).expect("exact limit");
    assert_eq!(pick(&maximum, [10.0, -20.0]), Pick::Blocked);
    assert_eq!(
        DisplayedFrame::new(state(map, vec![None; 161]), vec![161]).err(),
        Some(PickError::Invalid)
    );
    assert_eq!(
        DisplayedFrame::new(state(map, vec![None; 160]), vec![161]).err(),
        Some(PickError::Invalid)
    );
    let mut wrong_pose = state(map, vec![None]);
    wrong_pose.pose.clear();
    assert_eq!(
        DisplayedFrame::new(wrong_pose, vec![1]).err(),
        Some(PickError::Invalid)
    );
    let mut extra_pose = state(map, vec![None]);
    extra_pose.pose.push(identity());
    assert_eq!(
        DisplayedFrame::new(extra_pose, vec![1]).err(),
        Some(PickError::Invalid)
    );
    let empty = DisplayedFrame::new(state(map, vec![]), vec![0]).expect("empty view");
    assert_eq!(pick(&empty, [10.0, -20.0]), Pick::Miss);
}

#[test]
fn opaque_box_blocks_and_translucent_shell_uses_authored_half_threshold() {
    let vertices = box_vertices();
    let faces = BOX_TRIANGLES.each_ref().map(|t| Face {
        anchors: t[0],
        triangles: t,
    });
    let labelled = solid(&vertices, &faces);
    let mut front = labelled;
    front.pose = Transform::local([0.0, 0.0, 0.75], [0.0; 3], 1.0);
    let map = mapping([64; 2]);
    let at = pointer(map, [32.5; 2]);
    let bare = display(
        &[labelled],
        camera(),
        map,
        Quality::Motion,
        vec![Some(handle(0))],
    );
    assert_eq!(pick(&bare, at), Pick::Part(handle(0)));
    assert_eq!(pick(&bare, pointer(map, [0.5; 2])), Pick::Miss);
    for (opacity, expected) in [
        (1.0, Pick::Blocked),
        (0.49, Pick::Part(handle(0))),
        (0.5_f32.next_down(), Pick::Part(handle(0))),
        (0.5, Pick::Blocked),
    ] {
        front.opacity = opacity;
        let frame = display(
            &[labelled, front],
            camera(),
            map,
            Quality::Rest,
            vec![Some(handle(0)), None],
        );
        assert_eq!(pick(&frame, at), expected, "opacity {opacity}");
    }
    front.opacity = 1.0;
    front.pose = Transform::local([0.0, 0.0, 3.0], [0.0; 3], 1.0);
    let clipped = display(
        &[labelled, front],
        camera(),
        map,
        Quality::Motion,
        vec![Some(handle(0)), None],
    );
    assert_eq!(pick(&clipped, at), Pick::Part(handle(0)));
}

#[test]
fn sphere_bounding_box_corner_is_background() {
    use kumochi_scene::three::{
        Triangle,
        mesh::{self, MeshScratch},
    };
    let sphere =
        serde_json::from_str(r#"{"in":"view","id":"sphere","shape":"sphere","r":1,"segments":24}"#)
            .expect("sphere record");
    let mut triangles = Vec::<Triangle>::with_capacity(1280);
    mesh::build(
        &sphere,
        &identity(),
        0,
        &mut triangles,
        &AtomicBool::new(false),
        &mut MeshScratch::new(),
    )
    .expect("sphere mesh");
    let vertices: Vec<_> = triangles.iter().flat_map(|t| t.v).collect();
    let indices: Vec<_> = (0..triangles.len())
        .map(|i| [[3 * i as u32, 3 * i as u32 + 1, 3 * i as u32 + 2]])
        .collect();
    let faces: Vec<_> = indices
        .iter()
        .map(|t| Face {
            anchors: t[0],
            triangles: t,
        })
        .collect();
    let cam = Camera::new(0.0, 0.0, 4.0, 90.0, [0.0; 3]).expect("camera");
    let map = mapping([64; 2]);
    let frame = display(
        &[solid(&vertices, &faces)],
        cam,
        map,
        Quality::Rest,
        vec![Some(handle(0))],
    );
    assert_eq!(pick(&frame, pointer(map, [32.5; 2])), Pick::Part(handle(0)));
    assert_eq!(pick(&frame, pointer(map, [39.5, 24.5])), Pick::Miss);
}

#[test]
fn displayed_snapshot_stays_frozen_while_the_other_buffer_changes() {
    let vertices = box_vertices();
    let faces = BOX_TRIANGLES.each_ref().map(|t| Face {
        anchors: t[0],
        triangles: t,
    });
    let mut s = solid(&vertices, &faces);
    let old_map = mapping([64; 2]);
    let mut shown = display(
        &[s],
        camera(),
        old_map,
        Quality::Motion,
        vec![Some(handle(0))],
    );
    let old_allocation = shown.ids().as_ptr();
    let old_camera = camera();
    let mut desired_camera = old_camera;
    desired_camera.orbit(100.0, 20.0, 1.0).expect("orbit");
    s.pose = Transform::local([10.0, 0.0, 0.0], [0.0; 3], 1.0);
    let new_map = Mapping::new([0.0; 2], [500.0, 300.0], 2.0, [32; 2]).expect("mapping");
    let ready = display(
        &[s],
        desired_camera,
        new_map,
        Quality::Motion,
        vec![Some(handle(1))],
    );
    let (mut next_state, next_ids) = ready.into_parts();
    next_state.revision = 8;
    let ready = DisplayedFrame::new(next_state, next_ids).expect("next revision");
    assert_ne!(shown.ids().as_ptr(), ready.ids().as_ptr());
    assert_eq!(shown.state().camera, old_camera);
    assert_eq!(shown.state().pose, vec![identity()]);
    assert_eq!(shown.state().revision, 7);
    assert_eq!(shown.state().mapping, old_map);
    assert_eq!(
        pick(&shown, pointer(old_map, [32.5; 2])),
        Pick::Part(handle(0))
    );
    assert_eq!(pick(&ready, pointer(new_map, [16.5; 2])), Pick::Miss);
    let retired = std::mem::replace(&mut shown, ready);
    let (_, mut free_ids) = retired.into_parts();
    assert_eq!(free_ids.as_ptr(), old_allocation);
    free_ids.fill(1);
    assert_eq!(pick(&shown, pointer(new_map, [16.5; 2])), Pick::Miss);
    assert_eq!(shown.state().camera, desired_camera);
    assert_eq!(shown.state().pose, vec![s.pose]);
    assert_eq!(shown.state().revision, 8);
    assert_eq!(shown.state().mapping, new_map);
    assert_eq!(shown.state().parts, vec![Some(handle(1))]);
}

#[test]
fn empty_raster_misses_and_resolved_ids_keep_painted_translucency_and_majority_ties() {
    use kumochi_scene::three::raster::Fragment;
    let map = mapping([1; 2]);
    let empty = display(&[], camera(), map, Quality::Rest, vec![]);
    assert_eq!(pick(&empty, pointer(map, [0.5; 2])), Pick::Miss);
    for (points, expected) in [
        (
            [[0, 0], [256, 0], [256, 128], [0, 128]],
            Pick::Part(handle(0)),
        ),
        ([[0, 128], [256, 128], [256, 256], [0, 256]], Pick::Miss),
    ] {
        let mut out = FrameScratch::with_capacity(1);
        assert_eq!(
            out.push(&Fragment {
                points: &points,
                plane: [0.0, 0.0, 1.0],
                rgb: [255; 3],
                opacity: 1.0,
                solid: 1,
                centroid_z: 1.0
            }),
            Ok(true)
        );
        let (_, ids) = image(&mut out, [1; 2], Quality::Rest);
        let shown =
            DisplayedFrame::new(state(map, vec![Some(handle(0))]), ids).expect("resolved frame");
        assert_eq!(pick(&shown, pointer(map, [0.5; 2])), expected);
    }
    let mut out = FrameScratch::with_capacity(2);
    for (solid, depth, centroid) in [(1, 0.5, 100.0), (2, 0.25, 1.0)] {
        assert_eq!(
            out.push(&Fragment {
                points: &[[0, 0], [256, 0], [256, 256], [0, 256]],
                plane: [0.0, 0.0, depth],
                rgb: [255; 3],
                opacity: 0.5,
                solid,
                centroid_z: centroid
            }),
            Ok(true)
        );
    }
    let (_, ids) = image(&mut out, [1; 2], Quality::Rest);
    assert_eq!(ids, vec![2]);
    let shown =
        DisplayedFrame::new(state(map, vec![Some(handle(0)), None]), ids).expect("resolved frame");
    assert_eq!(pick(&shown, pointer(map, [0.5; 2])), Pick::Blocked);
}

#[test]
fn pick_has_zero_allocations_for_parts_blockers_misses_and_invalid_pointers() {
    let map = mapping([3, 1]);
    let frame =
        DisplayedFrame::new(state(map, vec![Some(handle(0)), None]), vec![1, 2, 0]).expect("frame");
    let pointers = [
        pointer(map, [0.5; 2]),
        pointer(map, [1.5, 0.5]),
        pointer(map, [2.5, 0.5]),
        [f64::NAN; 2],
    ];
    let (results, allocations) = allocation::count(|| pointers.map(|at| pick(&frame, at)));
    assert_eq!(
        results,
        [Pick::Part(handle(0)), Pick::Blocked, Pick::Miss, Pick::Miss]
    );
    assert_eq!(allocations, 0);
}

fn assert_ray_agreement(
    solids: &[Solid<'_>],
    cam: Camera,
    map: Mapping,
    quality: Quality,
) -> [usize; 4] {
    let triangles = world_triangles(solids);
    let parts: Vec<_> = (0..solids.len()).map(|i| Some(handle(i))).collect();
    let frame = display(solids, cam, map, quality, parts);
    let [width, height] = map.texels().map(f64::from);
    let [right, up, forward] = cam.basis();
    let eye = cam.eye();
    let focal = height * 0.5 / (cam.parameters()[3].to_radians() * 0.5).tan();
    let project = |p| {
        let p = delta(p, eye);
        let z = scalar(p, forward);
        [
            width * 0.5 + focal * scalar(p, right) / z,
            height * 0.5 - focal * scalar(p, up) / z,
        ]
    };
    let projected: Vec<_> = triangles.iter().map(|(t, _)| t.map(project)).collect();
    let mut samples = [0; 4];
    for y in 0..map.texels()[1] {
        for x in 0..map.texels()[0] {
            let p = [f64::from(x) + 0.2, f64::from(y) + 0.8];
            if projected
                .iter()
                .any(|t| (0..3).any(|i| edge_distance(p, t[i], t[(i + 1) % 3]) <= 1.0))
            {
                continue;
            }
            let ray = std::array::from_fn(|i| {
                forward[i] + (p[0] - width * 0.5) / focal * right[i]
                    - (p[1] - height * 0.5) / focal * up[i]
            });
            let mut hits: Vec<_> = triangles
                .iter()
                .filter_map(|(t, id)| ray_hit(eye, ray, *t).map(|z| (z, *id)))
                .collect();
            hits.sort_by(|a, b| a.0.total_cmp(&b.0));
            if hits.len() > 1 && (hits[0].0 - hits[1].0).abs() <= hits[0].0 * 1e-6 {
                continue;
            }
            let winner = hits.first().map_or(0, |&(_, id)| usize::from(id));
            let expected = if winner == 0 {
                Pick::Miss
            } else {
                Pick::Part(handle(winner - 1))
            };
            assert_eq!(
                pick(&frame, pointer(map, p)),
                expected,
                "camera {:?}, texel {p:?}, size {:?}",
                cam.parameters(),
                map.texels()
            );
            samples[winner] += 1;
        }
    }
    samples
}

#[test]
fn scaled_pointer_oracle_agrees_outside_one_internal_texel_and_declared_depth_ambiguity() {
    let vertices = box_vertices();
    let faces = BOX_TRIANGLES.each_ref().map(|t| Face {
        anchors: t[0],
        triangles: t,
    });
    let bars: [[Vec3; 8]; 3] = std::array::from_fn(|axis| {
        vertices.map(|v| {
            let mut p = [v.x, v.y, v.z];
            p[axis] *= 3.0;
            Vec3::new(p[0], p[1], p[2])
        })
    });
    let mut a = solid(&vertices, &faces);
    a.pose = Transform::local([-0.3, 0.0, 0.0], [0.0, 30.0, 0.0], 1.0);
    let mut b = solid(&vertices, &faces);
    b.pose = Transform::local([0.3, 0.2, 0.2], [20.0, 0.0, 0.0], 1.0);
    for solids in [
        vec![a, b],
        (0..3).map(|i| solid(&bars[i], &faces)).collect(),
    ] {
        for (size, quality, minimum) in [
            ([73, 59], Quality::Rest, 2000),
            ([37, 29], Quality::Motion, 400),
        ] {
            let mut seen = [0; 4];
            for view in 0..26 {
                let cam = Camera::new(
                    f64::from(view % 9) * 40.0,
                    f64::from(view / 9 - 1) * 40.0,
                    4.0,
                    if view == 25 { 120.0 } else { 60.0 },
                    [0.0; 3],
                )
                .expect("camera");
                let samples = assert_ray_agreement(&solids, cam, mapping(size), quality);
                assert!(
                    samples.iter().sum::<usize>() > minimum,
                    "{quality:?}, camera {view}: edge exclusions leave {samples:?}"
                );
                for (total, count) in seen.iter_mut().zip(samples) {
                    *total += count;
                }
            }
            assert!(
                seen[..=solids.len()].iter().all(|&count| count > 0),
                "{quality:?}: every solid and the background win rays across the sweep: {seen:?}"
            );
        }
    }
}

#[test]
fn oblique_120_degree_view_edge_and_near_plane_edge_match_the_ray_oracle() {
    let faces = [Face {
        anchors: SPLIT[0],
        triangles: &SPLIT,
    }];
    let vertices = SQUARE.map(|p| Vec3::new(p.x + 3.0, p.y, p.z));
    let wide = Camera::new(0.0, 0.0, 2.0, 120.0, [0.0; 3]).expect("camera");
    let map = mapping([64; 2]);
    let s = solid(&vertices, &faces);
    let shown = display(&[s], wide, map, Quality::Motion, vec![Some(handle(0))]);
    let p = [62.5, 32.5];
    let focal = 32.0 / (60.0_f64.to_radians()).tan();
    let ray = [(p[0] - 32.0) / focal, -(p[1] - 32.0) / focal, -1.0];
    assert!(
        world_triangles(&[s])
            .iter()
            .any(|(t, _)| ray_hit(wide.eye(), ray, *t).is_some()),
        "oblique ray hits the face"
    );
    assert_eq!(pick(&shown, pointer(map, p)), Pick::Part(handle(0)));

    let near = Camera::new(0.0, 0.0, 2.5, 90.0, [0.0; 3]).expect("camera");
    let clipped = [
        Vec3::new(-0.0625, -0.125, 2.46875),
        Vec3::new(0.25, -0.125, 2.0),
        Vec3::new(0.25, 0.125, 2.0),
        Vec3::new(-0.0625, 0.125, 2.46875),
    ];
    let s = solid(&clipped, &faces);
    let shown = display(&[s], near, map, Quality::Motion, vec![Some(handle(0))]);
    for (x, expected) in [
        (31.5, Pick::Miss),
        (32.0, Pick::Part(handle(0))),
        (32.5, Pick::Part(handle(0))),
    ] {
        let ray = [(x - 32.0) / 32.0, -0.5 / 32.0, -1.0];
        let hit = world_triangles(&[s])
            .iter()
            .filter_map(|(t, _)| ray_hit(near.eye(), ray, *t))
            .min_by(f64::total_cmp)
            .expect("source face ray");
        let oracle = if hit >= 0.125 {
            Pick::Part(handle(0))
        } else {
            Pick::Miss
        };
        assert_eq!(oracle, expected, "near-plane ray {x}");
        assert_eq!(
            pick(&shown, pointer(map, [x, 32.5])),
            expected,
            "near-plane pointer {x}"
        );
    }
}
