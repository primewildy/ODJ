// Linear-fader knob (cap) for the ODJ controller — Pioneer-style:
// low, wide, slightly chamfered on top with a finger-rest indent
// running across the cap (perpendicular to fader travel).
//
// Constraint from the fader hardware: the cap can extend at most
// 3.5 mm forward or backward of the stem along the travel axis, or
// the mounting screws at either end of the slider's travel will foul
// at extreme positions. So the cap length along travel is capped at
// 7 mm. Width (perpendicular to travel) is unconstrained — wider is
// easier to grip and adds nothing to the screw clearance problem.
//
// Stem (Alps/Bourns-style slide pot): 0.8 mm thin × 7.5 mm long ×
// 4.5 mm protrudes above the slider. The stem's LONG axis is
// perpendicular to fader travel (standard convention), so the slot's
// long axis aligns with the cap's WIDTH, not its length.
//
// Print right-side-up (top face up). The slot opens onto the build
// plate as a tall narrow rectangular hole — no overhangs, no support.

// ===== Dimensions (mm) =====

// Cap outer envelope. cap_l is the constrained axis.
cap_l = 7;      // along fader travel  (3.5 mm each side of stem)
cap_w = 14;     // perpendicular to travel — finger grip
cap_h = 8;      // total height above the slider's top face

// Stem socket — slip-fit with FDM tolerances.
stem_nominal_w = 0.8;    // narrow axis (along travel)
stem_nominal_l = 7.5;    // long axis (perpendicular to travel)
stem_protrude  = 4.5;
slot_clr_w = 0.3;
slot_clr_l = 0.4;
slot_w = stem_nominal_w + slot_clr_w;   // narrow → along Y (cap_l axis)
slot_l = stem_nominal_l + slot_clr_l;   // long   → along X (cap_w axis)
slot_depth = stem_protrude + 0.5;       // 0.5 mm extra so the cap bottoms on the slider, not the stem

// Top profile: 1.5 mm linear chamfer around the top edges, plus a
// shallow cross-cap finger groove giving the cap its Pioneer look and
// a tactile centre-line. The groove runs along the X axis
// (perpendicular to travel) so a finger naturally rests across it
// when pushing the cap up/down the fader slot.
chamfer       = 1.5;
groove_d      = 6;       // chord of the finger groove
groove_depth  = 1.2;

// ===== Geometry =====

module body() {
    // Linear chamfer on the top edges: hull between a full-size
    // rectangle at z = cap_h - chamfer and a shrunken one at z = cap_h.
    hull() {
        translate([-cap_w/2, -cap_l/2, 0])
            cube([cap_w, cap_l, cap_h - chamfer + 0.01]);
        translate([
            -(cap_w - 2 * chamfer)/2,
            -(cap_l - 2 * chamfer)/2,
            cap_h - 0.01
        ])
            cube([
                cap_w - 2 * chamfer,
                cap_l - 2 * chamfer,
                0.01
            ]);
    }
}

// Stem socket: rectangular hole rising from the bottom face. Long
// axis along X = perpendicular to fader travel (cap_w direction);
// narrow axis along Y = along travel (cap_l direction).
module stem_socket() {
    translate([-slot_l/2, -slot_w/2, -0.01])
        cube([slot_l, slot_w, slot_depth + 0.01]);
}

// Finger groove on the top face — a cylinder lying along the X axis
// (perpendicular to travel), scooped out so a finger rests across the
// cap centred over the stem.
module finger_groove() {
    r = (pow(cap_w/2, 2) + pow(groove_depth, 2)) / (2 * groove_depth);
    translate([0, 0, cap_h + r - groove_depth])
        rotate([0, 90, 0])
            cylinder(h = cap_w + 2, r = r, center = true, $fn = 64);
}

difference() {
    body();
    stem_socket();
    finger_groove();
}
