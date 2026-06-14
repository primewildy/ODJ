// Outer-case half for the ODJ controller.
//
// Two halves (LEFT + RIGHT) bolt together. Once the USB hole was
// pinned to the Pico's connector (asymmetric across the seam), the
// halves are no longer interchangeable, so we use that asymmetry:
//   - LEFT  half: USB cable exit + M3 bolt CLEARANCE holes at seam.
//   - RIGHT half: no USB hole + M3 heat-set INSERTS at seam.
// Set `side` below to "L" or "R" and re-render to export each STL.
//
// PCB sits horizontally between them, straddling the seam. Each
// half's INNER long wall is missing — that's the open face that
// meets its partner.
//
// Top-down view, assembled:
//
//   +----------------+----------------+
//   |                |                |   ← faceplate (separate, bolts on top)
//   |   Half A       |   Half B       |
//   |   (this file)  |   (this file,  |
//   |                |    flipped     |
//   |                |    in mirror)  |
//   +----------------+----------------+
//
// Print the part with the FLOOR on the build plate — i.e., right-side
// up. The open inner long wall (X = half_w) is then a side opening,
// printable without supports. The four corner pillars print solid
// because they extend the full height from floor to top.
//
// To assemble: print twice, slide PCB horizontally onto the M5
// posts (no inserts — M5 self-taps into the PETG/ABS print, light
// PCB doesn't need more), bolt the two halves together with 4 × M3
// hex bolts + nuts through the inner corner pillars (one bolt at
// each pillar's top + bottom = 4 total), then bolt the faceplate
// on top with 4 × M3 self-tap screws into the corner posts.

// ====================================================================
// Parameters
// ====================================================================

side          = "L";     // "L" (left, has USB + clearance holes) or
                         // "R" (right, has insert pockets, no USB)

half_w        = 125;     // X, half width (one deck's faceplate width)
half_l        = 210;     // Y, half length (one deck's faceplate length)
half_h        = 60;      // Z, total assembled height including faceplate
wall_t        = 3;       // outer wall thickness
floor_t       = 3;       // floor thickness
faceplate_t   = 3;       // (informational) — faceplate sits IN a top recess

// --- Corner posts (carry the faceplate + the seam bolts) ---
post_size     = 12;      // square cross-section, X = Y
post_h        = half_h - faceplate_t;   // up to where faceplate sits

// --- Faceplate corner screw mounts (mirror faceplate_deck.scad) ---
// Modelled for a brass heat-set insert: melt-in OD 5.5 mm, depth 8 mm.
// Hole is drawn 0.5 mm under that so the insert has plastic to displace
// as it sinks (standard heat-set practice — Ruthex / McMaster default).
fp_corner_inset = 6;     // from outer corner — matches faceplate's mount holes
fp_pilot_d      = 5;     // heat-set insert hole (target insert OD 5.5)
fp_pilot_depth  = 8;

// --- Inner-corner seam fastening (horizontal M3) ---
// LEFT half has a clearance hole through the full pillar depth.
// RIGHT half has a heat-set insert pocket on the seam-facing face.
// Bolt enters from inside the LEFT cavity, passes through the
// clearance hole, engages the insert on the RIGHT side.
seam_bolt_clr     = 3.5;   // M3 clearance through LEFT pillar
seam_insert_d     = 4;     // model hole for M3 heat-set insert (typical OD ~4.5)
seam_insert_depth = 5;     // insert pocket depth into the RIGHT pillar
seam_bolt_top_z   = post_h - 17;   // upper bolt (clears the faceplate pilot above)
seam_bolt_bot_z   = 10;            // lower bolt

// --- PCB mounts (horizontal, M5 heat-set insert) ---
// Posts only need to be tall enough to contain the heat-set insert
// + a couple mm of solid plastic at the base. 10 mm insert depth +
// 2 mm base = 12 mm post height. Putting the PCB this close to the
// floor leaves more headroom above for tall components (USB,
// JST headers) while the PCB itself has its underside 12 mm above
// the floor — plenty of clearance for solder fillets.
pcb_z         = 12;      // PCB bottom face Z (top of mount post)
pcb_post_x    = 81;      // half-local X (44 mm from seam — derived above)
// Y values are PCB-coord (30, 151) + 13.65 mm offset to centre the
// 135-mm-tall PCB in the 210-mm-long case. Clearance to the front /
// back pillars is ~25 mm at each end.
pcb_post_y_a  = 43.65;
pcb_post_y_b  = 164.65;
pcb_post_dia  = 12;      // boss outer — 3 mm wall around the 6 mm hole
pcb_pilot_d   = 6;       // heat-set insert hole (target insert OD 6.5)
pcb_pilot_depth = 10;

// --- USB cable cutout (full circle through the BACK wall only) ---
// LEFT half only — RIGHT half has solid back wall here.
//
// Position: 43 mm from the PCB's left outline edge (PCB-coord X = 29) =
//   PCB-coord X = 72 = case-X 119.7 (with the PCB centred on the seam
//   at case-X 125). Half-local X is identical: 119.7. The cylinder
//   edge at X = 124.7 is 0.3 mm clear of the seam — fully inside one
//   half so the hole renders as a complete circle.
//
// The cylinder length is wall_t + 0.5 mm: enough to punch cleanly
// through the wall without rendering slivers, just 0.25 mm into the
// pillar behind it — visually a wall-only cut.
usb_at_back   = true;    // true = Y = half_l, false = Y = 0
usb_d         = 10;
usb_x         = 119.7;   // half-local — aligns with Pico USB at PCB-X 72
usb_center_z  = pcb_z + 18;   // 18 mm above PCB bottom
usb_wall_h    = wall_t + 0.5; // cylinder length — wall only, just slop

// ====================================================================
// Geometry helpers
// ====================================================================

EPS = 0.01;

module corner_post(x, y) {
    translate([x - post_size/2, y - post_size/2, 0])
        cube([post_size, post_size, post_h]);
}

// Vertical M3 self-tap pilot from the post's top down — fastens faceplate.
module fp_pilot(x, y) {
    translate([x, y, post_h - fp_pilot_depth + EPS])
        cylinder(d = fp_pilot_d, h = fp_pilot_depth, $fn = 24);
}

// PCB mount boss — solid pillar from floor up to pcb_z, with M5 self-
// tap pilot drilled into the top.
module pcb_post(x, y) {
    translate([x, y, 0]) {
        cylinder(d = pcb_post_dia, h = pcb_z, $fn = 32);
    }
}
module pcb_pilot(x, y) {
    translate([x, y, pcb_z - pcb_pilot_depth + EPS])
        cylinder(d = pcb_pilot_d, h = pcb_pilot_depth, $fn = 24);
}

// Horizontal seam fastener at an inner-corner post. LEFT half gets a
// full-depth clearance hole (bolt slides through). RIGHT half gets a
// short insert pocket on the seam-facing face (bolt threads into the
// heat-set insert that lives here).
module seam_bolt(y, z) {
    if (side == "L") {
        translate([half_w + EPS, y, z]) rotate([0, -90, 0])
            cylinder(d = seam_bolt_clr, h = post_size + 2 * EPS, $fn = 24);
    } else {
        // Insert opens at the seam face (X = half_w) and extends inward
        // (toward X < half_w) by `seam_insert_depth`.
        translate([half_w + EPS, y, z]) rotate([0, -90, 0])
            cylinder(d = seam_insert_d, h = seam_insert_depth + EPS, $fn = 32);
    }
}

// ====================================================================
// Build
// ====================================================================

module case_half() {
    difference() {
        union() {
            // Floor.
            cube([half_w, half_l, floor_t]);
            // Outer wall (X = 0).
            cube([wall_t, half_l, post_h]);
            // Front wall (Y = 0).
            cube([half_w, wall_t, post_h]);
            // Back wall (Y = half_l).
            translate([0, half_l - wall_t, 0])
                cube([half_w, wall_t, post_h]);
            // Note: the inner long wall (X = half_w) is INTENTIONALLY
            // absent — that's the open face that meets the other half.
            //
            // Four corner posts running full height. They carry both
            // the faceplate screws and the seam bolts (at the inner
            // two posts).
            corner_post(post_size/2,             post_size/2);              // outer-front
            corner_post(post_size/2,             half_l - post_size/2);     // outer-back
            corner_post(half_w - post_size/2,    post_size/2);              // inner-front (seam)
            corner_post(half_w - post_size/2,    half_l - post_size/2);     // inner-back  (seam)
            // PCB mount bosses.
            pcb_post(pcb_post_x, pcb_post_y_a);
            pcb_post(pcb_post_x, pcb_post_y_b);
        }

        // ---- Faceplate corner screw pilot holes ----
        // Faceplate mounting holes (from faceplate_deck.scad) are at
        //   (corner_inset, corner_inset) etc., where corner_inset = 6.
        fp_pilot(fp_corner_inset,           fp_corner_inset);
        fp_pilot(half_w - fp_corner_inset,  fp_corner_inset);
        fp_pilot(fp_corner_inset,           half_l - fp_corner_inset);
        fp_pilot(half_w - fp_corner_inset,  half_l - fp_corner_inset);

        // ---- PCB self-tap pilots ----
        pcb_pilot(pcb_post_x, pcb_post_y_a);
        pcb_pilot(pcb_post_x, pcb_post_y_b);

        // ---- Seam bolts: two per inner pillar (top + bottom) ----
        // The bolts pass through THIS half's inner post and continue
        // into the mirrored half's post. With identical halves the two
        // pillars line up edge-to-edge at X = half_w.
        seam_bolt(post_size/2,              seam_bolt_top_z);
        seam_bolt(post_size/2,              seam_bolt_bot_z);
        seam_bolt(half_l - post_size/2,     seam_bolt_top_z);
        seam_bolt(half_l - post_size/2,     seam_bolt_bot_z);

        // ---- USB cable cutout — LEFT half only ----
        // Cylinder length = wall thickness + 0.5 mm slop, so it punches
        // through wall but barely (<0.25 mm) into the pillar behind it.
        if (side == "L") {
            usb_y = usb_at_back ? half_l - wall_t/2 : wall_t/2;
            translate([usb_x, usb_y, usb_center_z])
                rotate([90, 0, 0])
                    cylinder(d = usb_d, h = usb_wall_h, center = true, $fn = 32);
        }
    }
}

// Render one half. Edit `side` at the top of the file to switch
// between LEFT ("L") and RIGHT ("R") and re-export each STL.
//
// To preview both halves assembled, uncomment the visualisation block
// below — it draws the OTHER half mirrored alongside this one.
case_half();

// // --- visualisation only: mate of the chosen side ---
// color("LightBlue", 0.5) translate([2 * half_w, 0, 0])
//     mirror([1, 0, 0]) {
//         // temporary local override of `side` is awkward in SCAD —
//         // easier to comment the line above and uncomment one of:
//         // (run once with side="L", once with side="R")
//         case_half();
//     }
