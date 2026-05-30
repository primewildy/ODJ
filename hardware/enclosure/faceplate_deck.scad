// ODJ controller — single-deck 3D-printable faceplate
//
// Open in OpenSCAD (free, openscad.org). Tweak parameters at the top,
// then File → Render (F6) → File → Export → STL → slice → print.
// 3 mm panel, no overhangs, no supports needed. Print face-down for the
// nicest visible-face finish.
//
// Layout (looking at the panel, USB toward the back):
//
//     ┌───────────────────────────┐
//     │ ○ TL                  TR ○│   <- M3 corner mount holes
//     │     ┃          ●  HIGH    │   (encoder shaft + EQ_HIGH at top-right)
//     │  P  ┃          ●  MID     │   P = pitch fader (left, vertical)
//     │  I  ┃   JOG    ●  LOW     │   JOG = rotary encoder (centre)
//     │  T  ┃   (●)    ┃          │   E = volume fader (right, vertical)
//     │  C  ┃          ┃  V       │   pots stack above the volume fader
//     │  H  ┃          ┃  O       │
//     │     ┃          ┃  L       │
//     │                            │
//     │  (▷)  (◷)                  │   ▷ = Play (outside), ◷ = Cue (inside)
//     │ ○ BL                  BR ○│   <- M3 corner mount holes
//     └───────────────────────────┘
//
// All units: mm. Coordinates: origin = bottom-left, X right, Y up.

// ===== Overall plate =====
plate_w = 125;
plate_h = 240;
plate_t = 3;

// ===== Linear faders (pitch on left, volume on right) =====
// User-supplied: 65 mm between fader centres, 80 mm hole-to-hole mounting,
// M3 screws.
fader_spacing     = 65;     // centre-to-centre between the two faders
fader_mount_pitch = 80;     // hole-to-hole along the fader
fader_slot_len    = 60;     // travel slot (typical for 80 mm-mount fader)
fader_slot_w      = 4;      // slot width — clearance for the fader stem
fader_screw_d     = 3.2;    // M3 clearance

pitch_x  = 30;
pitch_cy = 150;             // slot midpoint
vol_x    = pitch_x + fader_spacing;
vol_cy   = 80;              // lower (EQ pots stack above)

// ===== Rotary encoder (jog) — between the two faders =====
// User-supplied: 6 mm shaft, 26 mm between mount holes, M3 screws.
enc_x           = (pitch_x + vol_x) / 2;
enc_y           = 170;      // upper-middle (visually central)
enc_shaft_d     = 7;        // 6 mm shaft + 1 mm clearance
enc_mount_pitch = 26;
enc_mount_axis  = "v";      // "v" = mount holes above/below shaft, "h" = left/right
enc_screw_d     = 3.2;

// ===== Rotary EQ pots (standard 9 mm Alpha-style, 2.7 mm anti-rotation tab) =====
// Stacked above the volume fader. HIGH at top, MID middle, LOW just above the fader.
pot_bushing_d  = 7.5;       // 7 mm bushing + 0.5 mm clearance
pot_tab_d      = 3.0;       // 2.7 mm tab + clearance
pot_tab_offset = 6.6;       // standard tab offset above shaft centre (12 o'clock)
pot_x          = vol_x;     // aligned with the volume fader
pot_y_low      = 145;
pot_y_mid      = 180;
pot_y_high     = 215;

// ===== Arcade buttons (Play + Cue), bottom-left =====
// 30 mm panel cutout for standard illuminated arcade buttons.
btn_hole_d  = 30;
btn_play_x  = 30;           // outside (leftmost, aligned with pitch fader)
btn_cue_x   = 70;           // inside (40 mm to the right of Play)
btn_y       = 45;

// ===== M3 corner mounting holes =====
corner_screw_d = 3.2;       // M3 clearance
corner_inset   = 6;         // inset from each edge

// ===== Geometry helpers =====
module screw_hole(d) circle(d=d, $fn=24);

module slot(w, l) hull() {
    translate([0, -l/2]) circle(d=w, $fn=24);
    translate([0,  l/2]) circle(d=w, $fn=24);
}

module fader(cx, cy) {
    translate([cx, cy]) slot(fader_slot_w, fader_slot_len);
    translate([cx, cy + fader_mount_pitch/2]) screw_hole(fader_screw_d);
    translate([cx, cy - fader_mount_pitch/2]) screw_hole(fader_screw_d);
}

module rotary_pot(cx, cy) {
    translate([cx, cy]) circle(d=pot_bushing_d, $fn=32);
    translate([cx, cy + pot_tab_offset]) circle(d=pot_tab_d, $fn=20);
}

module encoder(cx, cy) {
    translate([cx, cy]) circle(d=enc_shaft_d, $fn=40);
    if (enc_mount_axis == "v") {
        translate([cx, cy + enc_mount_pitch/2]) screw_hole(enc_screw_d);
        translate([cx, cy - enc_mount_pitch/2]) screw_hole(enc_screw_d);
    } else {
        translate([cx + enc_mount_pitch/2, cy]) screw_hole(enc_screw_d);
        translate([cx - enc_mount_pitch/2, cy]) screw_hole(enc_screw_d);
    }
}

module arcade_button(cx, cy) {
    translate([cx, cy]) circle(d=btn_hole_d, $fn=48);
}

module corner_mounts() {
    for (p = [[corner_inset,             corner_inset            ],
              [plate_w - corner_inset,   corner_inset            ],
              [corner_inset,             plate_h - corner_inset  ],
              [plate_w - corner_inset,   plate_h - corner_inset  ]])
        translate(p) screw_hole(corner_screw_d);
}

// ===== Build: extrude the plate, subtract all cutouts =====
linear_extrude(height = plate_t) difference() {
    square([plate_w, plate_h]);

    // Faders
    fader(pitch_x, pitch_cy);
    fader(vol_x,   vol_cy);

    // Encoder (jog)
    encoder(enc_x, enc_y);

    // EQ pots (right column, above volume fader)
    rotary_pot(pot_x, pot_y_low);
    rotary_pot(pot_x, pot_y_mid);
    rotary_pot(pot_x, pot_y_high);

    // Buttons (bottom-left)
    arcade_button(btn_play_x, btn_y);
    arcade_button(btn_cue_x,  btn_y);

    // Corner mounting holes
    corner_mounts();
}
