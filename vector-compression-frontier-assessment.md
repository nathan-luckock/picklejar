# Vector storage efficiency and adaptive compression: is the frontier open?

A disconfirmation-first assessment. For every "this is open," I state the strongest "already solved, and by whom." Negative-existence claims are labeled "no public evidence found," not proof of absence. Citation and inference are kept separate.

Date of research: June 2026. Asset under consideration: a from-scratch Rust relational-plus-vector engine, Postgres wire-compatible, HNSW index, Reed-Solomon erasure coding over the on-disk footprint, deterministic simulation testing and model-checking.

---

## Bottom line up front

The honest verdict, before the details:

1. **Pure storage efficiency (recall at fixed memory budget) is a crowded, fast-moving, largely-commoditized space.** RaBitQ, extended RaBitQ, binary-plus-rescore, ScaNN, LVQ/LeanVec, and now TurboQuant sit very close to the rate-distortion frontier. Beating the best published recall-at-memory by a margin a buyer cares about is **unlikely**. Confidence this is hard to win: **HIGH (~80%)**.

2. **Online drift-adaptive quantization (your hypothesized gap) is a real and acknowledged problem in production, but it is NOT unclaimed.** It is an active research front with at least three serious 2023-2025 entries, two of them from Meta and Amazon. The gap between "published research result" and "shipped, robust production feature in FAISS/ScaNN/Qdrant/Milvus" is the only real opening, and it is closing. Confidence the core idea is already claimed in research: **HIGH (~85%)**. Confidence there is still a production-grade opening: **MODERATE (~50%)**.

3. **Erasure-coding self-heal at the per-embedding / per-index-node granularity appears genuinely unclaimed in shipping vector DBs.** No public evidence found of a vector database that detects and reconstructs a single corrupted embedding rather than rebuilding the index or re-replicating a shard. But "unclaimed" here mostly means "nobody thought it was worth doing," because storage-layer EC and replication already cover durability. Confidence it is unclaimed: **MODERATE (~60%)**. Confidence it is *valuable* enough to win a buyer on its own: **LOW (~20%)**.

4. **The three-way combination (extreme density + index-layer self-heal + online drift adaptivity) is not offered by any single system I can find.** That is real white space as a *product*, but it is assemblable from published parts, so the combination alone is weakly defensible. Confidence no single system combines all three: **MODERATE (~65%)**.

**Probability that a focused 12-24 month effort produces a result beating published state of the art by a margin a buyer or acquirer would care about: roughly 15-25%.** Most of that probability mass lives in the drift-adaptivity axis on a *systems* benchmark (end-to-end recall stability under realistic drift with bounded I/O and no full reindex), not in raw recall-at-memory, where I put it closer to 5-10%. Reasoning for these numbers is in Question 9.

---

## Confidence legend

- **HIGH** = ~75-95%. Multiple independent sources, or a direct primary source with reproducible numbers.
- **MODERATE** = ~50-75%. One good source, or consistent secondary sources, or strong inference from primary facts.
- **LOW** = ~25-50%. Single weak source, or inference with meaningful unknowns.
- "No public evidence found" = I searched and did not find it. Not proof it does not exist.

---

## Q1. Vector quantization: state of the art and its ceiling

**What is achievable, with numbers and who holds them:**

The 1-bit-per-dimension family is the current frontier for extreme compression at high recall, and it is held by RaBitQ and its derivatives, not by classic PQ.

- **RaBitQ (Gao & Long, SIGMOD 2024)** quantizes to 1 bit/dimension with an unbiased distance estimator and a theoretical error bound that tightens as dimensionality grows (scales with 1/sqrt(D)). On GIST1M (960d), float32 is 3,840 bytes/vector; RaBitQ at 1 bit/dim is 120 bytes/vector, a 32x reduction, while PQ at m=64 is 64 bytes but with materially worse recall. Source: https://arxiv.org/pdf/2405.12497 and https://medium.com/@dnotitia/rabitq-1-bit-vector-quantization-part-3-92bbc1dbe8eb . Confidence: **HIGH (~90%)**.

- **Milvus IVF_RABITQ (2.6, production)** reports 94.7% recall at 3.6x the throughput of IVF_FLAT with memory at roughly 1/32 of the original vectors. With a RaBitQ primary plus SQ4/SQ6/SQ8 refine, Milvus reports ~95% recall. Source: https://milvus.io/blog/turboquant-rabitq-vector-database-cost.md and https://milvus.io/blog/milvus-26-preview-72-memory-reduction-without-compromising-recall-and-4x-faster-than-elasticsearch.md . Confidence: **HIGH (~85%)**.

- **Extended / multi-bit RaBitQ (Gao et al., SIGMOD 2025)** generalizes to B bits/dim. The RaBitQ library reports that 4-, 5-, and 7-bit configurations suffice for roughly 90%, 95%, and 99% recall respectively without reranking. Source: https://vectordb-ntu.github.io/RaBitQ-Library/ . Confidence: **HIGH (~85%)**.

- **Binary quantization plus rescoring (Qdrant, production)** on OpenAI text-embedding-3-large at 3072d: recall climbs from about 76-77% without rescoring to 97-99% with rescoring at 3x oversampling, at 32x compression. On ada-002 (1536d) it reports 0.98 recall@100 at 4x oversampling; on Cohere (4096d) 0.98 recall@50 at 2x oversampling. Source: https://qdrant.tech/articles/binary-quantization-openai/ and https://qdrant.tech/documentation/manage-data/quantization/ . Confidence: **HIGH (~85%)**.

- **TurboQuant (Google Research, ICLR 2026)** is a data-oblivious quantizer (random rotation then precomputed Lloyd-Max scalar quantization) that the authors report as within ~2.7x of the Shannon distortion-rate bound, compressing to 3-bit with little recall loss and beating PQ at every bit width tested, with effectively zero indexing time. Source: https://research.google/blog/turboquant-redefining-ai-efficiency-with-extreme-compression/ (summarized at https://decodethefuture.org/en/turboquant-vector-quantization-kv-cache/ and the Qdrant feature request https://github.com/qdrant/qdrant/issues/8524 ). **Caveat, important:** a follow-up note (April 2026) reports TurboQuant's published numbers do not reproduce from released artifacts and that RaBitQ matches or beats it on recall and runtime across tested datasets and bit widths. Source: https://arxiv.org/html/2604.19528v1 . So treat TurboQuant's "beats RaBitQ" framing as contested. Confidence TurboQuant is at least competitive: **MODERATE (~60%)**. Confidence the contest is unresolved: **HIGH (~80%)**.

- **LVQ and LeanVec (Intel SVS, production via FAISS and Redis)** are locally-adaptive scalar quantization (per-vector scaling after centering) and query-aware dimensionality reduction plus LVQ. LeanVec reports up to 2.4x over LVQ and 13.7x over HNSWlib at 10-recall@10 of 0.90, and ~2x over RoarANN (the NeurIPS 2023 Big-ANN out-of-distribution track winner). Source: https://arxiv.org/pdf/2312.16335 and https://github.com/facebookresearch/faiss/wiki/CPU-Faiss---Intel-SVS-%E2%80%90-Overview . Confidence: **HIGH (~80%)**.

- **Classic PQ / OPQ / additive (AQ, LSQ) / scalar** remain the baselines. They are still useful at moderate compression but lose to the rotation-plus-binary family at extreme compression. Confidence: **HIGH (~85%)**.

**Where quantization clearly hits a wall:**

1. **Rate-distortion floor.** TurboQuant's own framing (within ~2.7x of Shannon) tells you the ceiling: you cannot keep cutting bits and holding recall, because the information is genuinely gone. This is a hard mathematical wall, not an engineering one. Confidence: **HIGH (~85%)**, inference from the rate-distortion framing plus the convergence of multiple methods near the same frontier.

2. **Binary quantization collapses on low-dimensional or non-centered data.** Qdrant documents that BQ is only efficient for high-dimensional vectors with a centered component distribution and degrades below roughly 1024d. Source: https://github.com/qdrant/landing_page/blob/master/qdrant-landing/content/documentation/guides/quantization.md . Confidence: **HIGH (~85%)**.

3. **PQ can collapse outright on hard distributions.** On MSong, the RaBitQ paper reports PQ exceeding 50% average relative distance error and under 60% recall even with reranking, with recall abnormally decreasing as more buckets are probed. Source: https://arxiv.org/pdf/2405.12497 . Confidence: **HIGH (~85%)**.

**Takeaway:** The recall-at-memory frontier is occupied, contested, and close to a theoretical floor. The strongest single anchor to beat is RaBitQ / IVF_RABITQ at ~95% recall and ~32x compression.

---

## Q2. Learned compression beyond classic quantization

**Is there demonstrated headroom from neural or learned compression that beats PQ and binary at equal recall?** Partly, but mostly on the *model* side, not the *index* side, and the wins do not clearly beat the rotation-plus-binary family at equal recall post-hoc.

- **Matryoshka Representation Learning (MRL)** is the strongest practically-deployed "learned compression," but it is a training-time property of the embedding model, not a database technique. It nests information so you can truncate dimensions. LinkedIn reports going from 3072 to 512 dims with negligible recall loss (recall@10 0.4242 to 0.4225), with a sharp drop only at 50 dims (0.3716). Source: https://arxiv.org/pdf/2510.14223 . Voyage, Nomic, Arctic-Embed, and Qwen3 ship MRL plus quantization-aware training. Sources: https://www.mongodb.com/company/blog/technical/matryoshka-embeddings-smarter-embeddings-with-voyage-ai , https://arxiv.org/pdf/2412.04506 , https://arxiv.org/pdf/2601.04720 . Confidence MRL is real and adopted: **HIGH (~85%)**. **Key constraint:** MRL requires retraining or owning the embedding model. A database engine that does not control the embedder cannot apply MRL; it can only quantize what it is handed. This matters for you: it is the embedding-model vendors' moat, not the database's. Confidence on that constraint: **HIGH (~80%)**, inference.

- **Learned codebooks that beat PQ at equal recall do exist, and that is exactly what ScaNN's anisotropic quantization is.** It learns a quantizer that minimizes inner-product estimation error and reports ~2x over other libraries on ann-benchmarks. Source: https://research.google/blog/announcing-scann-efficient-vector-similarity-search/ . So "learned beats classic PQ" is true, but the winner is a 2020 Google result already in production, not open white space. Confidence: **HIGH (~85%)**.

- **Autoencoder / sparse-autoencoder compression** shows production wins in narrow settings. CompresSAE reports beating Matryoshka-based compression on downstream click-through at Recombee. Source: https://arxiv.org/html/2505.11388 . But these are recommender-specific and tied to a downstream objective, not general recall-at-memory wins over RaBitQ. Confidence learned/autoencoder beats RaBitQ at equal recall on standard ANN benchmarks: **LOW (~30%)**. No public evidence found of a learned post-hoc embedding compressor that beats extended RaBitQ at equal recall on SIFT1M/GIST1M/Deep1B at the same bit budget.

**Takeaway:** Classic-plus-rotation quantization is still the practical frontier for post-hoc compression of embeddings you do not own. Learned compression that wins is mostly training-time (MRL, QAT) and belongs to the embedding-model vendors. This is **not** open white space for a database engine. Confidence: **MODERATE-HIGH (~70%)**.

---

## Q3. Learned indexes for high-dimensional ANN

**Has the Kraska learned-index line been successfully applied to high-dimensional ANN, and does it beat HNSW/IVF/DiskANN/ScaNN?** Short answer: not in any production-relevant way.

- The Kraska et al. learned-index family (RMI, ALEX, PGM-index) targets 1D sorted keys, where a learned model predicts position in a sorted array. That structure does not transfer to high-dimensional ANN, where there is no total order to learn. Confidence: **HIGH (~85%)**, this is well-established and uncontested.

- The "learned ANN" wins that are real are **learned quantizers, not learned index structures**: ScaNN's anisotropic vector quantization (learning the codebook to minimize MIPS error) and SOAR (multiple cluster assignment as backup), which together won both the out-of-distribution and streaming tracks of the NeurIPS 2023 Big-ANN competition. Source: https://research.google/blog/soar-new-algorithms-for-even-faster-vector-search-with-scann/ . That is a real production advantage, owned by Google. Confidence: **HIGH (~85%)**.

- No public evidence found of a learned-index *data structure* (in the RMI/ALEX sense) that beats HNSW/IVF/DiskANN/ScaNN on standard high-dimensional ANN benchmarks at production scale. There are research papers applying learned models to routing or partitioning, but nothing that has displaced graph or IVF indices on the leaderboards.

**What is genuinely open here:** very little that a from-scratch engine can win. The "learned" wins are quantizer-side and already taken by ScaNN. Confidence this axis is not an opening: **MODERATE-HIGH (~70%)**.

---

## Q4. Adaptivity under distribution drift (your hypothesized gap)

This is the one you most need, so it gets the most detail. The headline: **the index-structure freshness problem is solved; the quantization-codebook drift problem is being actively solved by Meta, Amazon, and Intel, and is acknowledged as an open limitation in production docs.** Your specific phrasing, "an index whose compression continuously tracks the live distribution without full reindex," is the exact thing CoDEQ and DeDrift attack.

**Part A: index-structure freshness under inserts/deletes is solved.** Confidence: **HIGH (~85%)**.

- **FreshDiskANN (Microsoft, 2021)** maintains a billion-point index with thousands of concurrent inserts/deletes/searches per second while retaining >95% 5-recall@5, at 5-10x lower freshness cost than rebuild. Source: https://arxiv.org/abs/2105.09613 .
- **SPFresh (2023)**, **IP-DiskANN (2025, in-place updates)**, **Quake**, **CleANN**, **UBIS** all push streaming insert/delete with stable recall, several beating FreshDiskANN on deletion time and stability. Sources: https://www.researchgate.net/publication/374920073_SPFresh_Incremental_In-Place_Update_for_Billion-Scale_Vector_Search , https://arxiv.org/abs/2502.13826 , https://sheng.whu.edu.cn/papers/25bigdata.pdf .
- So "the set of indexed vectors changes and the graph stays fresh" is well-covered. **This is not your gap.**

**Part B: quantization-codebook drift is the actual question, and it is being actively claimed.** Confidence the core idea is claimed in research: **HIGH (~85%)**.

- **DeDrift (Meta / FAIR, 2023)** explicitly targets robust similarity search under content drift. Its Hybrid variant nearly matches a full rebuild's recall while being 160-250x cheaper than full reconstruction on YFCC and VideoAds. It re-clusters the IVF as clusters grow stale. Source: https://arxiv.org/pdf/2308.02752 . This is a direct, published, Meta-authored attack on your gap.

- **CoDEQ (Amazon, December 2025)** is the most direct and most recent competitor. It formally studies data-dependent quantization under streaming updates, defines a "dynamic consistency" property (after every update, answers match a from-scratch rebuild), and proves you can update a data-dependent quantizer with O(1) consecutive disk I/Os per update while retaining static accuracy guarantees. Empirically it maintains roughly constant recall under drifting inserts/deletes while FrozenPQ, RaBitQ, OnlinePQ, and even DeDriftPQ decline. Source: https://arxiv.org/pdf/2512.18335 . This is, almost exactly, "an index whose compression continuously tracks the live distribution without full reindex," with a theoretical framework, from Amazon. If you build toward this gap, **CoDEQ is the paper you are competing with.**

- **IVF-TQ / streaming TurboQuant** uses a codebook-free residual (fixed rotation plus precomputed scalar quantization) specifically to remove the staleness failure mode of trained-codebook indexes. On streaming Deep-10M it holds 87.4% to 86.6% recall (-0.8pp) while IVF-PQ degrades -3.23pp, and per-batch PQ retraining does not close the gap. Source: the streaming-quantization line discussed at https://qdrant.tech/articles/turboquant-quantization/ and https://kiadev.net/news/2026-05-20-turbovec-rust-vector-turboquant . Note: there is already a **Rust** implementation of TurboQuant (Turbovec) that markets exactly the "no codebook retraining under drift" property. So even the Rust-implementation angle is partly taken. Confidence: **MODERATE (~65%)**.

- **Intel's locally-adaptive quantization for streaming (LVQ, 2024)** addresses dynamic indices, and Intel's own LVQ is "locally adaptive" per vector. Source: https://arxiv.org/pdf/2402.02044 .

- **The production reality, in the vendors' own words.** Redis's SVS documentation states drift plainly as an unsolved general limitation: if incoming vector characteristics shift over time, compression quality degrades, and this is a general limitation of all data-dependent compression methods, with the learned representation becoming less effective as data stops resembling the training sample. Source: https://redis.io/docs/latest/develop/ai/search-and-query/vectors/svs-compression/ . **This is the single most useful sentence for your thesis:** the shipping systems acknowledge they degrade under drift and effectively recommend retrain/rebuild. So the production gap is real even though the research gap is largely claimed.

**Synthesis of Q4.** Your instinct is correct that production systems do not robustly self-adapt their quantization to drift; they degrade and lean on periodic rebuild (Redis says so explicitly). But your instinct that this is *unclaimed* is wrong: DeDrift (Meta, 2023) and CoDEQ (Amazon, 2025) are exactly this, with theory and benchmarks. The opening is narrow and specific: the research exists but is **not yet a robust, shipped, default-on feature inside FAISS / ScaNN / Qdrant / Milvus**, and CoDEQ in particular is brand-new (Dec 2025) and not yet productized. A from-scratch engine could plausibly be first to ship a genuinely drift-adaptive quantizer as a real feature. That is a **productization and integration** win, not a novel-algorithm win. Confidence the productization opening exists right now: **MODERATE (~50%)**, and decaying month by month as the big players integrate this work.

---

## Q5. Erasure coding and self-healing at the vector / index granularity

**Does any vector DB erasure-code or self-heal the index or embeddings themselves, rather than delegating to storage or replicating whole shards?**

- No public evidence found of a shipping vector database that erasure-codes individual embeddings or index nodes and detects-and-reconstructs a single corrupted embedding rather than rebuilding the index or re-replicating a shard. The erasure-coding art I found is storage-layer: disk arrays, distributed object stores, and the classic self-healing-data-store patents from the 2000s. Sources: https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/7681104 and https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/7681105 . Vector DBs (Milvus, Qdrant, Pinecone, Weaviate) get durability from replication plus the underlying object store (which itself may be EC'd, for example S3 or Ceph). Confidence the per-embedding index-layer EC self-heal is unclaimed in vector DBs: **MODERATE (~60%)**. The hedge: durability is often handled a layer down and not advertised, so absence of marketing is weak evidence.

- **The harder truth: "unclaimed" probably means "not worth claiming."** Embeddings are typically reconstructable from the source documents (you re-embed), and the index is rebuildable. Page-level checksums plus replication plus storage-layer EC already give you durability. Detecting and reconstructing one corrupted embedding in place is elegant but rarely the bottleneck a buyer is paying to solve. Your erasure coding is genuine engineering rigor, and it differentiates on *correctness story*, but I would not bet the company on it as the headline. Confidence it is a weak standalone wedge: **MODERATE (~60%)**, inference.

- Where it does have teeth: the threat model is shifting. There is active discussion of vector-store poisoning and silent corruption, where modifying a tiny fraction of vectors degrades retrieval invisibly to standard metrics. Sources (secondary, treat as directional): https://arxiv.org/pdf/2603.09002 and https://medium.com/@BuildShift/your-vector-databases-arent-safe-anymore-05d22ea90e83 . If you frame your EC self-heal as *integrity and tamper-detection at the embedding level* rather than disk durability, it lands on a live concern. That is a positioning move, and I am inferring the market interest, not citing demand. Confidence: **LOW-MODERATE (~40%)**.

---

## Q6. The combination

**Does any single system combine extreme density + index-layer self-heal + online drift adaptivity?**

- No public evidence found of a single system offering all three. Confidence: **MODERATE (~65%)**. The negative is plausible because the three properties come from three different communities (quantization research, storage/systems, and streaming-ANN research) that rarely ship in one engine.

- **But the combination is assemblable from published parts.** Extreme density (RaBitQ/extended RaBitQ, open source), drift adaptivity (DeDrift open from FAIR, CoDEQ's method published, TurboQuant data-oblivious), and durability (storage-layer EC, replication). A motivated team at Milvus or Qdrant could assemble a close approximation in a few quarters. So the combination is **real white space as a product** but **weakly defensible as IP**, because none of the three legs is novel and the integration, while hard, is not a moat. Confidence the combination is not by-itself defensible: **MODERATE (~60%)**, inference.

- The defensible version is not "we have all three," it is "we have a *measurably better* drift-adaptive quantizer with a correctness story nobody else can match because of our deterministic-simulation and model-checking rigor." The moat is the rigor and the benchmark, not the feature list.

---

## Q7. The number you would have to beat

**Concrete anchors, strongest current methods:**

- **Recall-at-memory anchor:** RaBitQ / IVF_RABITQ at roughly **95% recall and ~32x compression** (~1 bit/dim plus light refine). Binary-plus-rescore reaches **97-99% recall at 32x** on high-dim OpenAI/Cohere embeddings. Extended RaBitQ gives **90/95/99% recall at 4/5/7 bits/dim**. These are the walls. Sources as in Q1.

- **Drift anchor:** CoDEQ maintains roughly **constant recall under streaming drift** while RaBitQ/FrozenPQ/OnlinePQ/DeDriftPQ decline, at **O(1) disk I/Os per update**. IVF-TQ holds **-0.8pp on streaming Deep-10M vs IVF-PQ's -3.23pp**. Sources as in Q4.

**What counts as a meaningful improvement:**

- On pure recall-at-memory: a 10% memory reduction at equal recall is **noise**; nobody re-platforms for it. A clean **2x at equal recall over RaBitQ** would be a real result and very hard, likely impossible, given the rate-distortion floor. An order of magnitude is not physically available at these recall levels. So the bar for "buyer cares" on this axis is roughly 2x, and the ceiling for what is achievable is roughly 0x (you are already near the floor). That mismatch is why this axis is a bad bet. Confidence: **MODERATE-HIGH (~70%)**, inference from the rate-distortion convergence.

- On drift: the meaningful target is **end-to-end recall stability under realistic drift with bounded I/O and no full reindex, beating CoDEQ and DeDrift on a named streaming benchmark** (Big-ANN streaming track, or the DeDrift YFCC/VideoAds setups, or a Deep1B drift split). Here a few points of sustained recall advantage, or the same recall at materially lower update I/O, is genuinely meaningful, because the incumbents currently degrade and rebuild. This is the only axis where a new entrant's achievable margin and the buyer-relevant margin overlap. Confidence: **MODERATE (~55%)**.

---

## Q8. The strongest disconfirming case, then rebuttal

**The disconfirming case (steelmanned):**

FAISS (Meta), ScaNN (Google), DiskANN (Microsoft), Qdrant, and Milvus are mature, heavily optimized, open source, and backed by trillion-dollar companies and well-funded startups. Quantization is commoditized: RaBitQ is open and already integrated into Milvus, FAISS, Qdrant, Weaviate, and LanceDB within roughly a year of publication. The frontier sits near the rate-distortion floor, so "store more per dollar" has almost no headroom left. The drift problem is being solved by the very incumbents you would compete with (Meta's DeDrift, Amazon's CoDEQ, Intel's streaming LVQ, ScaNN/SOAR winning the Big-ANN streaming track). A from-scratch Rust engine, however clean, has no structural advantage in linear algebra kernels (everyone uses the same SIMD/AVX-512 tricks), no data-distribution advantage, and no embedding-model advantage. "Store more per dollar" is a feature these incumbents iterate on continuously and ship faster than a solo or small team can. The benchmark leaderboards are won by teams of specialists with GPU budgets. On this reading, you would be entering a race that is both near its physical ceiling and densely staffed by the people who wrote the methods you would be reimplementing.

**Rebuttal:**

1. **Research result is not shipped feature.** CoDEQ is from December 2025 and not productized; DeDrift is open but not a default in mainstream vector DBs; Redis openly documents that its compression degrades under drift. There is a real lag between leaderboard and product, and that lag is where a focused entrant can be first-to-ship a robust drift-adaptive quantizer. This rebuttal is **moderately strong**.

2. **The moat is correctness, not kernels.** Your deterministic simulation testing and from-scratch model-checking are unusual and genuinely valuable in a category where silent corruption and poisoning are live concerns. "Provably correct vector engine that self-heals and does not degrade under drift" is a *trust* pitch, and trust is undervalued on throughput-obsessed leaderboards. This rebuttal is **moderately strong** but **commercially unproven**, since I found no evidence buyers currently pay a premium specifically for this.

3. **Postgres wire compatibility plus relational-plus-vector in one engine is a distribution wedge** that pure ANN libraries do not have. This is real, but it is a *go-to-market* advantage, not a storage-efficiency one, and it competes with pgvector, which is entrenched. This rebuttal is **weak on the specific question asked** (storage efficiency frontier) even though it may matter most for the actual business.

**Where the rebuttal is weak, stated honestly:** On the literal question "can a from-scratch engine beat the published recall-at-memory state of the art," the rebuttal mostly fails. You probably cannot, and you should not try. The rebuttal only works if you redefine the contest from "best recall-at-memory" to "first robust, correct, drift-adaptive vector engine with a relational front end," which is a product and trust contest, not a benchmark contest. If your acquirer is buying a benchmark number, the disconfirming case wins. If your acquirer is buying a correct, durable, drift-resistant *system*, the rebuttal has a path.

---

## Q9. Bottom line and probability

**Is there a genuine, defensible open frontier a new from-scratch engine could win on a measurable benchmark in the next 12-24 months?**

- **Pure vector-storage efficiency (recall at fixed memory): no.** It is near the rate-distortion floor, commoditized, and owned by RaBitQ/ScaNN/LVQ. Do not pick this fight. Confidence: **HIGH (~80%)**.

- **Learned compression: no, not for a database that does not own the embedding model.** The wins are training-time (MRL, QAT) and belong to the embedding vendors. Confidence: **MODERATE-HIGH (~70%)**.

- **Drift adaptivity: narrowly, maybe.** The research is claimed (DeDrift, CoDEQ) but not yet shipped as a robust default in mainstream engines, and the vendors admit they degrade under drift. This is the one place where achievable margin and buyer-relevant margin overlap.

**The single sharpest benchmarkable target:** sustained recall under realistic distribution drift, on a named streaming benchmark (NeurIPS Big-ANN streaming track, DeDrift's YFCC/VideoAds, or a Deep1B drift split), at a fixed memory budget and bounded update I/O, with no full reindex, **beating CoDEQ and DeDrift**. The specific edge to claim is not "lower bits" but "we hold recall flat under drift where they decline, and we prove the engine never silently corrupts while doing it." The correctness rigor is the differentiator that converts a tie on the algorithm into a win on the system.

**Probability a focused 12-24 month effort beats published state of the art by a margin a buyer or acquirer would care about: ~15-25% overall.**

- Recall-at-memory axis alone: **~5-10%**. You are racing physics and Meta/Google.
- Drift-adaptivity axis on a systems benchmark: **~25-35%** *conditional on* you target the streaming/drift benchmark specifically, lead with the correctness story, and accept that you are competing with a 6-month-old Amazon paper that will likely be productized by incumbents during your build window.

The dominant risk is not that you fail to build something good. It is that CoDEQ-class methods get absorbed into FAISS, Milvus, and Qdrant before you ship, collapsing your window. The dominant opportunity is that none of the incumbents currently combines drift-adaptive quantization with a provably-correct, self-healing, Postgres-compatible *system*, and that combination, sold as trust rather than as a benchmark number, is not on anyone's roadmap that I can find.

If the decision is binary "build a better quantizer or not," the honest answer is **lean no.** If the decision is "extend an already-strong correct engine toward being the first drift-adaptive, self-healing, relational-plus-vector system, and benchmark drift specifically," the honest answer is **a real but minority-odds yes**, and the value is in the system and the trust story, not in beating RaBitQ on bits.

---

## Sources

Primary / strongest:
- RaBitQ (SIGMOD 2024): https://arxiv.org/pdf/2405.12497
- Extended RaBitQ library: https://vectordb-ntu.github.io/RaBitQ-Library/
- RaBitQ vs TurboQuant reproducibility note (2026): https://arxiv.org/html/2604.19528v1
- CoDEQ, quantization under streaming updates (Amazon, Dec 2025): https://arxiv.org/pdf/2512.18335
- DeDrift (Meta/FAIR, 2023): https://arxiv.org/pdf/2308.02752
- FreshDiskANN (Microsoft, 2021): https://arxiv.org/abs/2105.09613
- IP-DiskANN in-place updates (2025): https://arxiv.org/abs/2502.13826
- ScaNN anisotropic VQ (Google, ICML 2020): https://research.google/blog/announcing-scann-efficient-vector-similarity-search/
- SOAR (Google, 2024): https://research.google/blog/soar-new-algorithms-for-even-faster-vector-search-with-scann/
- LeanVec (Intel): https://arxiv.org/pdf/2312.16335
- Locally-adaptive quantization for streaming (Intel, 2024): https://arxiv.org/pdf/2402.02044
- Matryoshka at LinkedIn scale: https://arxiv.org/pdf/2510.14223

Production / vendor:
- Milvus IVF_RABITQ: https://milvus.io/blog/turboquant-rabitq-vector-database-cost.md
- Milvus 2.6 RaBitQ preview: https://milvus.io/blog/milvus-26-preview-72-memory-reduction-without-compromising-recall-and-4x-faster-than-elasticsearch.md
- Qdrant binary quantization on OpenAI: https://qdrant.tech/articles/binary-quantization-openai/
- Qdrant quantization docs: https://qdrant.tech/documentation/manage-data/quantization/
- Qdrant TurboQuant: https://qdrant.tech/articles/turboquant-quantization/
- FAISS Intel SVS overview: https://github.com/facebookresearch/faiss/wiki/CPU-Faiss---Intel-SVS-%E2%80%90-Overview
- Redis SVS compression (drift limitation stated): https://redis.io/docs/latest/develop/ai/search-and-query/vectors/svs-compression/
- Weaviate 8-bit rotational quantization: https://weaviate.io/blog/8-bit-rotational-quantization
- TurboQuant (Google Research): https://research.google/blog/turboquant-redefining-ai-efficiency-with-extreme-compression/
- Turbovec (Rust TurboQuant): https://kiadev.net/news/2026-05-20-turbovec-rust-vector-turboquant

Storage-layer EC (for the self-heal comparison):
- Self-healing erasure-coded data store patent: https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/7681104
- Lock-free clustered erasure coding patent: https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/7681105

Secondary / directional (vector-store integrity and poisoning, treat as directional, not load-bearing):
- Multi-agent vector-store security: https://arxiv.org/pdf/2603.09002
- Vector DB poisoning overview: https://medium.com/@BuildShift/your-vector-databases-arent-safe-anymore-05d22ea90e83
