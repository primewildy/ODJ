// Quick test print — one arcade button hole, one encoder hole, one
// M3 screw hole on a thin strip. Same dimensions as faceplate_deck
// but everything else stripped out so it prints in a few minutes.
//
// Goal: verify the three cutout diameters fit their hardware before
// committing to a full faceplate print.

plate_w = 66;
plate_h = 36;
plate_t = 1.5;                // half the production plate (3 mm) — only
                              // needs to be stiff enough to hold the
                              // parts while you check fitment

btn_hole_d   = 30;            // arcade button panel cutout
enc_flange_d = 20;            // rotary encoder flange clearance
screw_d      = 3.2;           // M3 clearance

// 3 mm of material between every hole edge → smallest the strip can
// go without compromising stiffness at 1.5 mm thickness.
btn_cx = 18;                  // 30 mm hole, 3 mm to left edge
enc_cx = 46;                  // 20 mm hole, 3 mm to arcade edge
screw_cx = 61;                // M3 hole, 3 mm to encoder edge

cy = plate_h / 2;             // everything on the horizontal midline

linear_extrude(height = plate_t) difference() {
    square([plate_w, plate_h]);
    translate([btn_cx,   cy]) circle(d=btn_hole_d,   $fn=48);
    translate([enc_cx,   cy]) circle(d=enc_flange_d, $fn=48);
    translate([screw_cx, cy]) circle(d=screw_d,      $fn=24);
}
