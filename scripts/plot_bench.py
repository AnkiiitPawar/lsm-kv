"""Plot docs/bench.json (from `cargo run --release --example bench`). Needs matplotlib."""

import json

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

d = json.load(open("docs/bench.json"))
fig, (a, b, c) = plt.subplots(1, 3, figsize=(14, 3.9))

bits = [r[0] for r in d["bloom"]]
a.semilogy(bits, [r[3] for r in d["bloom"]], "-", color="#c0504d", label="theory (1 - e^(-k/b))^k")
a.semilogy(bits, [r[2] for r in d["bloom"]], "o", color="#1f5f99", label="measured, 10^6 probes")
a.set(xlabel="bits per key (b)", ylabel="false-positive rate", title="Bloom filter accuracy")
a.legend(frameon=False, fontsize=8)

curve = d["amplification_curve"]
b.plot([x / 1000 for x, _ in curve], [y for _, y in curve], color="#1f5f99")
b.set(xlabel="keys written (thousands)", ylabel="bytes to disk per user byte", title="Write amplification")
for i, (t, size) in enumerate(d["levels"]):
    b.text(0.97, 0.30 - 0.1 * i, f"L{i}: {t} tables, {size / 1e6:.1f} MB", transform=b.transAxes, ha="right", fontsize=8)

labels = ["key present", "key absent"]
consulted = [d["hit_tables_consulted"], d["miss_tables_consulted"]]
read = [d["hit_blocks_read"], d["miss_blocks_read"]]
x = range(2)
c.bar([i - 0.18 for i in x], consulted, 0.36, color="#9fb7d0", label="tables whose key range covers the key")
c.bar([i + 0.18 for i in x], read, 0.36, color="#1f5f99", label="blocks actually read (after Bloom filters)")
for i in x:
    c.text(i + 0.18, read[i] + 0.05, f"{read[i]:.3f}", ha="center", fontsize=8)
c.set(xticks=list(x), xticklabels=labels, ylabel="per lookup", title="Read cost per point lookup", ylim=(0, 6.2))
c.legend(frameon=False, fontsize=7, loc="upper right")
for ax in (a, b, c):
    ax.spines[["top", "right"]].set_visible(False)
fig.tight_layout()
fig.savefig("docs/bench.png", dpi=120)
print("wrote docs/bench.png")
