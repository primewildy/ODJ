// Jog-encoder knob for the ODJ controller.
//
// Encoder shaft: 5.5 mm diameter with a single D-flat, 14.5 mm of
// shaft protrudes above the panel. The encoder mounts on a 30 mm PCD
// with three M3 cap-head screws; we recess the bottom of the knob so
// the screw heads have somewhere to live.
//
// Print upside-down on the bed (top face down) so the dimple, the
// chamfered top edge and the screw recess all print without supports.
// 0.2 mm layer height in the user's standard ABS profile.

// ===== Dimensions (mm) =====
od            = 40;     // outer diameter
height        = 20;     // total height
recess_d      = 35;     // diameter of bottom recess (clears mount screws on a 30 mm PCD)
recess_h      = 4;      // depth of bottom recess

shaft_nominal = 5.5;    // encoder shaft diameter
shaft_clr     = 0.2;    // diametric clearance for FDM tolerances
shaft_d       = shaft_nominal + shaft_clr;
shaft_flat    = 4.5;    // standard D-flat: flat is this far from the opposite edge of the shaft
shaft_depth   = 14.5;   // matches protruding shaft length

chamfer       = 3;      // top-edge slope: linear chamfer from od down to (od - 2*chamfer)
dimple_d      = 6;      // finger / marker dimple on the top face
dimple_depth  = 1.5;
dimple_offset = od/2 - 6;   // dimple centre, measured from knob centre

// ===== Geometry =====

// Main body with a chamfered top edge. The chamfer is a linear taper
// from the full OD at z = (height - chamfer) up to (od - 2*chamfer)
// at z = height.
module body() {
    union() {
        cylinder(d = od, h = height - chamfer, $fn = 120);
        translate([0, 0, height - chamfer])
            cylinder(
                d1 = od,
                d2 = od - 2 * chamfer,
                h  = chamfer,
                $fn = 120
            );
    }
}

// D-shaped shaft hole. Start with a clearance-fit circle, then
// subtract the half-plane that creates the flat.
module shaft_hole() {
    flat_offset = shaft_nominal/2 - (shaft_nominal - shaft_flat);  // shaft side of the flat
    intersection() {
        // The "circular" part of the D.
        translate([0, 0, -0.01])
            cylinder(d = shaft_d, h = shaft_depth + 0.02, $fn = 48);
        // Cut the flat: leave everything on the -x side of x = flat_offset.
        // Slight pull-in of the hole's flat (clr/2) so the shaft seats snug.
        translate([-od, -od, -0.02])
            cube([od + flat_offset - shaft_clr/2, 2 * od, shaft_depth + 0.04]);
    }
}

// Screw-clearance recess on the bottom face.
module screw_recess() {
    translate([0, 0, -0.01])
        cylinder(d = recess_d, h = recess_h + 0.01, $fn = 120);
}

// Finger / position-indicator dimple on the top face. A sphere
// subtracted from above gives a clean hemispherical scoop without
// supports if the part is printed upside-down.
module dimple() {
    // Sphere radius derived from chord (dimple_d) + sagitta (dimple_depth).
    r = (pow(dimple_d/2, 2) + pow(dimple_depth, 2)) / (2 * dimple_depth);
    translate([dimple_offset, 0, height + r - dimple_depth])
        sphere(r = r, $fn = 48);
}

difference() {
    body();
    shaft_hole();
    screw_recess();
    dimple();
}
