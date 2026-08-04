# Experimental voxel compositor

SNESRecomp includes an optional, presentation-only software compositor for
voxel experiments. A game describes a rectangle of its final ARGB8888 frame as
a grid and supplies a callback that assigns each cell a height.

`snes_voxel_render()` projects the live pixels onto prism tops and shaded
sides, with either an orbit camera or an explicit camera pose. The compositor
runs after the emulated PPU and does not change CPU, PPU, input, timing, or
save-state data. Games that never call it retain the normal rendering path.

The screen-grid API is deliberately policy-free. A game can derive height from
its collision/tile data, from final-pixel material statistics, or from a blend
of both. It can also preserve native rows after composition for an authored
HUD.
