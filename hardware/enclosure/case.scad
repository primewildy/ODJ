// Single-piece outer case for the ODJ controller.
//
// Supersedes case_half.scad now that a Voron with a 350 × 350 bed
// can print the whole shell in one go. The two faceplates from
// faceplate_deck.scad bolt onto the top of this part unchanged —
// the 8 corner pilot holes here line up with the 4+4 mount holes
// the user already drilled in those faceplates.
//
// Assembled top-down view:
//
//   +------+-----+--+-----+------+
//   |  ●   |  ●  ‖  ●  |   ●    |   ← faceplate mount pilots (×8)
//   |      |     ‖     |        |
//   |  faceplate A | faceplate B |
//   |      |     ‖     |        |
//   |  ●   |  ●  ‖  ●  |   ●    |
//   +------+-----+--+-----+------+
//        125 mm         125 mm
//        ← left ─→     ← right →
//
// The two inner "seam" pillars (at X = 119 and X = 131) used to
// be separate halves' inner posts. They're now adjacent solid
// pillars in one shell — touching faces union seamlessly so the
// 24 × 12 footprint reads as a single block in the print.
//
// Print orientation: floor on the build plate (Z up). No supports
// needed — the cavity opens onto the top, and the corner posts
// stand free.
//
// PCB still straddles the case along Y. Mount posts are M5 self-tap
// into the bare plastic (no inserts).

// ====================================================================
// Parameters
// ====================================================================

case_w      = 250;     // X — two 125 mm faceplates side by side
case_l      = 210;     // Y
case_h      = 60;      // Z, total assembled height including faceplate
wall_t      = 3;       // outer wall thickness
floor_t     = 3;       // floor thickness
faceplate_t = 3;       // faceplate sits IN a top recess; informational

// --- Corner posts (carry the 8 faceplate pilots) ---
post_size   = 12;
post_h      = case_h - faceplate_t;

// --- Faceplate corner screw mounts (mirror faceplate_deck.scad) ---
// M3 brass heat-set insert: melt-in OD 5.5 mm, depth 8 mm. Hole drawn
// 0.5 mm under that so the insert has plastic to displace.
fp_corner_inset = 6;
fp_pilot_d      = 5;
fp_pilot_depth  = 8;

// Faceplate mount-hole XY positions (case coords). Two faceplates of
// 125 × 210 sit at (0..125) and (125..250). Each has a hole 6 mm in
// from each of its corners.
fp_holes = [
    // Left faceplate
    [fp_corner_inset,                       fp_corner_inset],
    [125 - fp_corner_inset,                 fp_corner_inset],
    [fp_corner_inset,                       case_l - fp_corner_inset],
    [125 - fp_corner_inset,                 case_l - fp_corner_inset],
    // Right faceplate
    [125 + fp_corner_inset,                 fp_corner_inset],
    [case_w - fp_corner_inset,              fp_corner_inset],
    [125 + fp_corner_inset,                 case_l - fp_corner_inset],
    [case_w - fp_corner_inset,              case_l - fp_corner_inset],
];

// --- PCB mounts ---
// One PCB straddles the case. 4 mount posts, M5 self-tap into the
// post top (no heat-set — the lightweight PCB doesn't need it).
pcb_z         = 12;
pcb_post_dia  = 12;
pcb_pilot_d   = 6;       // target insert OD 6.5
pcb_pilot_depth = 10;

// PCB-coord X = 30 / Y = (30, 151) in the original two-half layout.
// In assembled coords that puts the four mount posts at:
pcb_posts = [
    [81,            43.65],
    [81,            164.65],
    [case_w - 81,   43.65],   // 169
    [case_w - 81,   164.65],
];

// --- USB cable cutout (BACK wall) ---
// At X = 119.7 = aligned with the Pico USB connector. In the old
// half design this position was right at the seam between two
// pillars and only had to punch through the back wall (≈3.5 mm).
// In the single-piece case there is now a SOLID corner post at
// X = 113..125 directly behind the wall — it carries the inner-
// back faceplate pilot at (119, 204). So the cutout has to reach
// THROUGH that post (12 mm deep) too, or the cable hits plastic.
usb_at_back   = true;
usb_d         = 10;
usb_x         = 119.7;
usb_center_z  = pcb_z + 18;
usb_punch_len = wall_t + post_size + 1;  // 16 mm: wall + post + slop

// ====================================================================
// Helpers
// ====================================================================

EPS = 0.01;

module corner_post(cx, cy) {
    translate([cx - post_size/2, cy - post_size/2, 0])
        cube([post_size, post_size, post_h]);
}

module fp_pilot(cx, cy) {
    translate([cx, cy, post_h - fp_pilot_depth + EPS])
        cylinder(d = fp_pilot_d, h = fp_pilot_depth, $fn = 24);
}

module pcb_post(cx, cy) {
    translate([cx, cy, 0])
        cylinder(d = pcb_post_dia, h = pcb_z, $fn = 32);
}

module pcb_pilot(cx, cy) {
    translate([cx, cy, pcb_z - pcb_pilot_depth + EPS])
        cylinder(d = pcb_pilot_d, h = pcb_pilot_depth, $fn = 24);
}

// ====================================================================
// Build
// ====================================================================

module case() {
    difference() {
        union() {
            // Floor.
            cube([case_w, case_l, floor_t]);
            // Four outer walls.
            cube([wall_t, case_l, post_h]);                                     // X = 0
            translate([case_w - wall_t, 0, 0])
                cube([wall_t, case_l, post_h]);                                 // X = case_w
            cube([case_w, wall_t, post_h]);                                     // Y = 0
            translate([0, case_l - wall_t, 0])
                cube([case_w, wall_t, post_h]);                                 // Y = case_l
            // 8 corner posts under the 8 faceplate pilots. The two
            // pairs along X = 119 / X = 131 touch at X = 125; OpenSCAD
            // unions them into a single 24 × 12 block.
            for (p = fp_holes) corner_post(p[0], p[1]);
            // PCB mount bosses.
            for (p = pcb_posts) pcb_post(p[0], p[1]);
        }

        // ---- Faceplate corner pilot holes (×8) ----
        for (p = fp_holes) fp_pilot(p[0], p[1]);

        // ---- PCB self-tap pilots ----
        for (p = pcb_posts) pcb_pilot(p[0], p[1]);

        // ---- USB cable cutout (back wall + inner pillar) ----
        // Cylinder lies along Y, starts a sliver outside the back
        // wall and grows inward by usb_punch_len so it clears both
        // the wall AND the corner pillar that sits behind it.
        if (usb_at_back) {
            translate([usb_x, case_l + EPS, usb_center_z])
                rotate([90, 0, 0])
                    cylinder(d = usb_d, h = usb_punch_len, $fn = 32);
        } else {
            translate([usb_x, -EPS, usb_center_z])
                rotate([-90, 0, 0])
                    cylinder(d = usb_d, h = usb_punch_len, $fn = 32);
        }
    }
}

case();
