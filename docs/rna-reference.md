# RNA access and references

RNA traversal begins at `bpy.context` or `bpy.data`. Public RNA metadata is the
allowlist: attributes beginning with `_` and functions absent from
`bl_rna.functions` are rejected except for a small set of collection helpers and
`keyframe_insert` / `keyframe_delete` on actual `bpy_struct` instances. `rna-describe`
includes these available methods alongside RNA metadata.

Pointer and collection values become opaque references. References carry a
generation and type. File load, undo/redo, deletion, and destructive collection
changes can invalidate them. A stale reference is an expected recoverable
condition; reacquire the root or parent selector.

The generation is a document epoch. A file load invalidates old references even
when the replacement document reuses the same object names. Saving a checkpoint
copy does not change that epoch. `reference-stats` reports retained handles;
`reference-release!` releases handles the caller no longer needs.

The bridge retains up to 65,536 handles. When it is full it evicts the least
recently used eighth, never handles from the request in progress; using an evicted
handle returns `stale_reference`, so fetch it again. Only a single result that alone
needs more handles than that fails, with `reference_limit`.

Array-backed subdata handles, such as mesh vertices or layers, are invalidated
conservatively after geometry or shading updates. Node-tree data -- nodes, sockets,
links, and interface items -- is allocated item by item and survives them. Delayed
Blender callbacks can also invalidate handles after native operations; reacquire
them when needed. ID references, rooted collections, and modifiers survive these
updates. Modifier references resolve through the owning object's session identity and Blender's
[`persistent_uid`](https://docs.blender.org/api/5.2/bpy.types.Modifier.html#bpy.types.Modifier.persistent_uid),
with a pointer check for replacement. Thus `add-modifier!` results remain usable
after geometry evaluation and renaming. Destructive collection edits still expire
the owner's modifier handles; removing the owner also makes them stale. This is not a precise
log of edits made outside MCP; load and undo/redo still change the whole document
epoch.

Use `rna-property-info` and `rna-function-info` for one property's constraints or
one permitted method's signature. Discovery uses the attached Blender's metadata,
including the explicitly supported collection and keyframe helpers.

Serialization is bounded by depth, item count, and bytes. Complex/cyclic values
become references rather than recursively expanding forever.
