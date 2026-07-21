# T4: Conflict Prediction + Software Graph ML Evaluation — Research Findings

## Evidence Table

| # | Source | URL | Key Claim | Type | Confidence |
|---|--------|-----|-----------|------|------------|
| 1 | Owhadi-Kareshk et al., "Predicting Merge Conflicts in Collaborative Software Development" (ESEM 2019) | https://arxiv.org/abs/1907.06274 | F1 0.95–0.97 for safe merges, 0.57–0.68 for conflicts; 9 Git feature sets; 267K merge scenarios from 744 repos | primary | high |
| 2 | Dias et al., "On the Prediction of Software Merge Conflicts" (ACM 2023, systematic review) | https://dl.acm.org/doi/fullHtml/10.1145/3592813.3592931 | 50% of conflict prediction studies use ML; hybrid multi-factor approaches recommended | secondary | high |
| 3 | Al-Refai et al., "Merge Conflict Prediction Using Feature Selection and Stacking Heterogeneous Ensembles" (JSMR 2025) | https://doi.org/10.1002/smr.70047 | Stack-SVM best ensemble; social + technical features both important; DT best individual model | primary | high |
| 4 | Vale et al., "Predicting merge conflicts considering social and technical assets" (EMSE 2023) | https://link.springer.com/article/10.1007/s10664-023-10395-8 | Social features (developer identity, team overlap) improve conflict prediction beyond technical-only | primary | medium |
| 5 | Ziegler, "GITCoP: ML-Based Approach to Predicting Merge Conflicts from Repository Metadata" (Master's thesis) | https://www.se.cs.uni-saarland.de/theses/ThomasZieglerMA.pdf | Repository metadata features for conflict prediction | primary | medium |
| 6 | ConE: Concurrent Edit Detection (Microsoft, FSE 2021) | https://ar5iv.labs.arxiv.org/html/2101.06542 | Deployed on 234 Microsoft repos; 26K PRs assessed, 775 recommendations, >70% rated useful; uses Extent of Overlap + Rarely Concurrently Edited files | primary | high |
| 7 | Berrendorf et al., "A Unified Framework for Rank-based Evaluation Metrics for Link Prediction in KGs" (2022) | https://arxiv.org/abs/2203.07544 | Unified framework for MRR, Hits@k; proposes adjusted metrics for interpretability | primary | high |
| 8 | Sun et al., "Revisiting the Evaluation Protocol of KG Completion Methods" (WWW 2021) | https://dl.acm.org/doi/fullHtml/10.1145/3442381.3449856 | Standard ranking protocol critique; filtered vs raw setting matters | primary | high |
| 9 | Bordes et al., "Translating Embeddings for Modeling Multi-relational Data" (NeurIPS 2013) | https://proceedings.neurips.cc/paper_files/paper/2013/file/1cecc7a77928ca8133fa24680a88d2f9-Paper.pdf | TransE: foundational KG embedding; entity ranking evaluation protocol | primary | high |
| 10 | Ali et al., "Bringing Light Into the Dark: Large-scale Evaluation of KGE Models" (PyKEEN) | https://backend.orbit.dtu.dk/ws/files/262633429/Bringing_Light_Into_the_Dark_A_Large_scale_Evaluation_of_Knowledge_Graph_Embedding_Models_under_a_Unified_Framework.pdf | Re-implemented 21 KGE models in PyKEEN; many results not reproducible with reported hyperparams | primary | high |
| 11 | Yan et al., "Athena: Enhancing Code Understanding for Impact Analysis by Combining Transformers and PDGs" (FSE 2024) | https://doi.org/10.1145/3643770 | mRR 60.32%, mAP 35.19%, HIT@10 81.48%; CodeBERT + program dependence graphs; 25 OSS projects | primary | high |
| 12 | Zaraket et al., "Change propagation using Temporal Graphs and GNNs" (IST 2024) | https://dl.acm.org/doi/10.1016/j.infsof.2023.107368 | Temporal Graph Network + LSTM for file-level change propagation; outperforms prior work | primary | high |
| 13 | Mehta et al., "Rex: Preventing Bugs via Correlated Change Analysis" (NSDI 2020, Microsoft) | https://www.usenix.org/system/files/nsdi20-paper-mehta.pdf | Correlated change analysis in large services; detects missing co-changes to prevent bugs | primary | high |
| 14 | Hoang et al., "DeepJIT: End-to-End Deep Learning for JIT Defect Prediction" (MSR 2019) | https://doi.org/10.1109/msr.2019.00016 | CNN on commit messages + code changes; end-to-end, no manual features; outperforms traditional JIT | primary | high |
| 15 | Milewicz et al., "JITGNN: Deep GNN for Just-In-Time Bug Prediction" (JSS 2024) | https://doi.org/10.1016/j.jss.2024.111984 | GNN on ASTs of changed programs; same AUC as JITLine (state-of-art); graph structure doesn't improve over sequence | primary | high |
| 16 | Schesch et al., "Resolve Merge Conflicts with LLMs" + Merge-Bench dataset (2025) | https://arxiv.org/html/2605.25890v1 | 7938 hunks from 1439 repos, 11 languages; LLMergeJ 14B via GRPO; best models <60% correct resolution | primary | high |
| 17 | Merge-Bench GitHub (evaluation toolkit) | https://github.com/benedikt-schesch/Merge-Bench | Public benchmark for merge conflict resolution; 5 evaluation metrics | primary | high |
| 18 | MergeConflictBench (Create Inc, 2024) | https://github.com/Create-Inc/merge-conflict-bench | 86 real conflicts from React/Next.js/React Native production code | primary | medium |
| 19 | ConGra dataset (HKU, 2024) | https://github.com/HKU-System-Security-Lab/ConGra | Large-scale multilingual conflict resolution dataset | primary | medium |
| 20 | AgenticFlict dataset (Zenodo, 2025) | https://zenodo.org/records/20118379 | Merge conflicts from AI coding agent PRs on GitHub | primary | medium |
| 21 | CoEdPilot (ISSTA 2024) | https://arxiv.org/html/2408.01733v1 | Edit location accuracy 70.8–85.3%, content exact match 41.8%, BLEU4 60.7; ripple effect estimation via neural transformers; 180K commits from 471 projects | primary | high |
| 22 | NES: Next Edit Suggestion (2025) | https://arxiv.org/html/2508.02473v2 | Instruction-free, low-latency edit prediction from historical editing trajectories; dual-model architecture | primary | medium |
| 23 | GRACE: Graph-Guided Repository-Aware Code Completion (2025) | https://arxiv.org/html/2509.05980v1 | Uses call chains and inheritance hierarchies for retrieval in repo-level completion | primary | medium |
| 24 | GNNContext: GNN-based Code Context Prediction (TSE 2025) | https://doi.org/10.1109/tse.2025.3578390 | GNN for predicting code context relevant to programming tasks | primary | medium |
| 25 | striff-gnn: GNN for Software Architecture Analysis | https://github.com/hadi-technology/striff-gnn | Heterogeneous Graph Transformer for masked edge reconstruction on code graphs; Java/Python/TypeScript | primary | medium |
| 26 | Mohamed et al., "Popularity Agnostic Evaluation of KGE" (AISTATS 2020) | https://proceedings.mlr.press/v124/mohamed20a/mohamed20a.pdf | Standard Hits@k and MRR biased toward popular entities; proposes strat-hits@k and strat-mrr | primary | high |
| 27 | Inconsistency among evaluation metrics in link prediction (PMC 2024) | https://pmc.ncbi.nlm.nih.gov/articles/PMC11574622/ | Significant inconsistency across LP evaluation metrics on 100s of real networks | primary | high |

---

## 1. Merge Conflict Prediction — Methods and Results

### 1.1 State of the Art

The dominant approach to merge conflict prediction uses **supervised ML classifiers on repository metadata features** extracted via Git commands [1][3][4][5]. A systematic review found that 50% of published conflict prediction studies use ML techniques, with the remainder using Dependency-based Automatic Locking (30%) or Behavior-Driven Development (10%) [2].

**Key results from Owhadi-Kareshk et al. (2019)** [1]:
- **Dataset**: 267,657 merge scenarios from 744 GitHub repositories in 7 languages (C, C++, C#, Java, PHP, Python, Ruby)
- **Features**: 9 lightweight Git feature sets (extractable solely through Git commands)
- **Results**: F1 = 0.95–0.97 for predicting *safe* (non-conflicting) merges; F1 = 0.57–0.68 for predicting *conflicting* merges
- **Conclusion**: Conflict prediction is feasible as a pre-filter for speculative merging, but predicting actual conflicts (the minority class) remains challenging

**Al-Refai et al. (2025)** [3] evaluated stacking heterogeneous ensembles:
- Models compared: DT, SVM (linear), Naive Bayes (3 variants), Logistic Regression, MLP, SGD, KNN
- Best individual model: Decision Tree (DT)
- Best ensemble: Stack-SVM (stacking with SVM meta-learner)
- Finding: Both **social features** (developer identity, team overlap) and **technical features** (file overlap, change size) are important; using all features outperforms feature-selected subsets

**Vale et al. (2023)** [4] confirmed that **social and technical assets combined** improve conflict prediction beyond technical-only approaches.

### 1.2 Non-ML Baselines

**ConE (Microsoft, 2021)** [6] is a production-deployed concurrent edit detection system that does NOT use ML:
- **Heuristic-based**: Uses two constructs: Extent of Overlap (EOO ≥ 50%) and Rarely Concurrently Edited files (RCE ≥ 2)
- **Scale**: Deployed on 234 Microsoft repositories; assessed 26,000 PRs
- **Results**: 775 recommendations, >70% (554) rated useful by developers
- **User satisfaction**: 90% of 48 interviewed users intended to keep using it daily
- **Key insight**: Concurrent edits to files correlate more with bug fixes than non-concurrent edits (Spearman correlation)

**Traditional baselines** (non-ML):
- **Textual overlap detection**: Check if same lines/regions are modified in both branches
- **Dependency analysis**: Static call graph / import analysis to detect structural conflicts
- **Speculative merging**: Continuously merge all branch combinations in background (expensive but accurate) [1]

---

## 2. Link Prediction Evaluation Framework

### 2.1 Standard Metrics for Knowledge Graph Completion

The standard evaluation protocol for KG link prediction follows the **entity ranking procedure** introduced by Bordes et al. (TransE, 2013) [9]:

For each test triple (h, r, t):
1. Replace head entity with every entity → score all candidates
2. Replace tail entity with every entity → score all candidates
3. Rank the correct entity among all candidates
4. Report rank-based metrics

**Core metrics** [7][8][9]:

| Metric | Definition | Range | Interpretation |
|--------|-----------|-------|----------------|
| **Mean Rank (MR)** | Average rank of correct entity | [1, \|E\|] | Lower is better; sensitive to outliers |
| **Mean Reciprocal Rank (MRR)** | Average of 1/rank | (0, 1] | Higher is better; emphasizes top ranks |
| **Hits@k** | Fraction of correct entities ranked in top k | [0, 1] | Higher is better; k ∈ {1, 3, 10} standard |
| **AUC** | Area under ROC curve | [0, 1] | Classification-oriented; less common for ranking |

### 2.2 Filtered vs. Raw Setting

The **filtered setting** (standard) removes other known-true triples from the ranking, preventing correct triples from being counted as errors [8][9]. The **raw setting** does not filter, leading to artificially worse metrics.

### 2.3 Known Issues and Improvements

**Berrendorf et al. (2022)** [7] proposed a unified theoretical framework and identified problems:
- Metrics are not directly comparable across datasets of different sizes
- Proposed adjusted variants for interpretability

**Mohamed et al. (2020)** [26] showed that **standard Hits@k and MRR are biased toward popular entities** (high-degree nodes). They proposed **strat-hits@k** and **strat-mrr** as unbiased estimators.

**PMC study (2024)** [27] found **significant inconsistency** across evaluation metrics on hundreds of real networks — different metrics can rank algorithms differently.

### 2.4 Applicability to Dependency DAGs

For Atomic's dependency DAGs, the relevant adaptations are:

- **MRR and Hits@k** are directly applicable for predicting missing dependency edges or co-change links
- The **filtered setting** is essential since many true dependencies exist
- **AUC** is appropriate when framing as binary classification (will-conflict / won't-conflict)
- For temporal dependency prediction, evaluation should be **time-split** (train on past, test on future) rather than random split
- **Strat-MRR** [26] is advisable since popular files (utility modules) will dominate standard metrics

### 2.5 Reproducibility Warning

Ali et al. (PyKEEN, 2021) [10] re-implemented 21 KGE models and found many published results were **not reproducible** with reported hyperparameters, underscoring the importance of rigorous evaluation protocol.

---

## 3. Blast-Radius / Change Impact Analysis

### 3.1 Academic Approaches

**Athena (FSE 2024)** [11] — most directly relevant:
- **Method**: Combines Transformer-based code embeddings (CodeBERT, UniXCoder, GraphCodeBERT) with Program Dependence Graphs (PDGs)
- **Task**: Given a changed method, predict which other methods are impacted
- **Results**: mRR 60.32%, mAP 35.19%, HIT@10 81.48% on benchmark of 25 OSS projects
- **Improvement**: 10.34% mRR, 9.55% mAP, 11.68% HIT@10 over simpler baseline (statistically significant)
- **Key insight**: Combining structural (graph) and semantic (embedding) information significantly outperforms either alone

**Temporal Graph approach (IST 2024)** [12]:
- **Method**: Models software as temporal graph (nodes = files, edges = co-changeability); uses Temporal Graph Network + LSTM
- **Task**: Predict which files will be impacted by a modification to a given file
- **Key innovation**: Temporal graph representation captures evolving dependencies, not just static snapshots
- **Result**: Significantly outperforms prior static-graph approaches

**Rex (Microsoft, NSDI 2020)** [13]:
- **Method**: Correlated change analysis for large services
- **Task**: Detect when a code change requires a correlated configuration change (or vice versa)
- **Application**: Preventing bugs from missing co-changes in production services
- **Production deployment**: Used at Microsoft scale

### 3.2 JIT Defect Prediction (Related)

**DeepJIT (MSR 2019)** [14]:
- **Method**: End-to-end CNN on commit messages + code change diffs
- **Task**: Predict if a commit will introduce a defect (just-in-time)
- **Innovation**: No manual feature engineering; learns directly from raw commit data
- **Result**: Outperforms traditional JIT approaches using hand-crafted features

**JITGNN (JSS 2024)** [15]:
- **Method**: GNN on Abstract Syntax Trees (ASTs) of changed code
- **Task**: Same JIT bug prediction as DeepJIT
- **Result**: Same AUC as JITLine (state-of-art sequence model)
- **Notable finding**: Graph structure did NOT improve over sequence-based models for this task — important negative result for our design

### 3.3 Tool-based Approaches (Industry)

Several open-source blast-radius tools exist but are **static analysis, not ML-based**:
- Call graph traversal + PageRank-style risk propagation (phoenix-assistant/blastradius)
- Transitive dependency tracing (ehermanson/blast-radius)
- AI-augmented call chain analysis (tazwaryayyyy/BlastRadius using IBM Bob)

These serve as useful baselines for comparison with learned approaches.

---

## 4. Code Change Recommendation / Next-Edit Prediction

### 4.1 CoEdPilot (ISSTA 2024) [21]

Most comprehensive system for edit recommendation with ripple-effect awareness:
- **Architecture**: Orchestrates multiple neural transformers:
  - Edit-propagating File Locator (coarse-grained)
  - Edit-propagating Line Locator (fine-grained)
  - Edit-dependency Analyzer (prior edit relevance)
  - Edit-content Generator
- **Training data**: 180K commits from 471 open-source projects, 5 languages
- **Results**:
  - Edit location accuracy: 70.8%–85.3%
  - Edit content exact match: 41.8%
  - BLEU4 score: 60.7
  - Boosts GRACE and CoditT5 by 8.57% exact match and 18.08 BLEU4
- **Relevance to Atomic**: The "ripple effect estimation" component maps directly to blast-radius prediction

### 4.2 NES: Next Edit Suggestion (2025) [22]

- Instruction-free, low-latency framework using learned historical editing trajectories
- Dual-model architecture: one for edit location, one for edit content
- Captures developer goals implicitly from edit history

### 4.3 GRACE (2025) [23]

- Graph-guided repository-aware code completion
- Uses call chains and inheritance hierarchies for retrieval
- Addresses limitation of pure text-similarity RAG approaches

### 4.4 GNNContext (TSE 2025) [24]

- GNN for predicting code context elements relevant to programming tasks
- Leverages structural information of code (not just text)

### 4.5 striff-gnn [25]

- Heterogeneous Graph Transformer (HGT) encoder trained via masked edge reconstruction
- Learns structural representations of codebases across Java, Python, TypeScript
- **Directly relevant**: Masked edge reconstruction = link prediction on software graphs

---

## 5. Available Datasets and Benchmarks

### 5.1 Merge Conflict Datasets

| Dataset | Size | Languages | Source | URL |
|---------|------|-----------|--------|-----|
| **Merge-Bench** [16][17] | 7,938 hunks from 1,439 repos | 11 languages | GitHub (Reaper + top-starred) | https://github.com/benedikt-schesch/Merge-Bench |
| **MergeConflictBench** [18] | 86 conflicts | React/Next.js/React Native | Production code | https://github.com/Create-Inc/merge-conflict-bench |
| **ConGra** [19] | Large-scale | Multilingual | GitHub | https://github.com/HKU-System-Security-Lab/ConGra |
| **AgenticFlict** [20] | Large-scale | Multiple | AI coding agent PRs | https://zenodo.org/records/20118379 |
| **Owhadi-Kareshk dataset** [1] | 267,657 merge scenarios | 7 languages (C/C++/C#/Java/PHP/Python/Ruby) | 744 GitHub repos (Reaper) | (available via paper) |

### 5.2 Impact Analysis Benchmarks

| Dataset | Size | Granularity | Source | URL |
|---------|------|-------------|--------|-----|
| **Athena benchmark** [11] | 25 OSS projects | Method-level | Bug-fix commits | https://github.com/yanyanfu/Athena |

### 5.3 KG Link Prediction Benchmarks (Reference)

Standard benchmarks: FB15k-237, WN18RR, YAGO3-10 — these are the standard KGE evaluation datasets [9][10]. While not directly applicable, the evaluation protocols transfer to software dependency graph prediction.

---

## 6. Baseline Methods for Comparison

### 6.1 For Conflict Prediction

| Baseline | Type | Expected Performance | Notes |
|----------|------|---------------------|-------|
| Textual overlap (same lines modified) | Rule-based | High precision, low recall | Catches only syntactic conflicts |
| File-level overlap counting | Heuristic | Moderate | ConE's approach [6] |
| Git feature classifiers (RF/DT) | ML | F1 0.57–0.68 on conflicts [1] | Current SOTA for prediction |
| Speculative merging | Exact | 100% precision/recall | Computationally expensive |

### 6.2 For Blast-Radius Estimation

| Baseline | Type | Expected Performance | Notes |
|----------|------|---------------------|-------|
| Static call graph traversal | Structural | Conservative (over-estimates) | No learning, deterministic |
| Co-change mining (association rules) | Statistical | Data-dependent | Requires change history |
| Coupling metrics (structural/evolutionary) | Metric-based | mRR ~50% (Athena baseline) [11] | Traditional IA approaches |

### 6.3 For Link Prediction on Software Graphs

| Baseline | Type | Notes |
|----------|------|-------|
| Common Neighbors | Topological | Simple, often strong baseline |
| Jaccard Coefficient | Topological | Normalized overlap |
| Adamic-Adar | Topological | Weighted common neighbors |
| TransE [9] | KGE | Translational embedding |
| DistMult | KGE | Bilinear diagonal |
| Node2Vec + classifier | GNN-adjacent | Random walk embeddings |

---

## 7. Key Takeaways for Atomic Neural Graph Design

1. **Conflict prediction is feasible but asymmetric** [1]: Safe merges are easy to predict (F1 ~0.96), but actual conflicts are hard (F1 ~0.63). This suggests the system should focus on **filtering likely-safe scenarios** rather than precisely predicting conflicts.

2. **Graph structure alone may not help** [15]: JITGNN showed that GNN on ASTs achieved the same AUC as sequence models for JIT prediction. Graph structure must be combined with semantic information (as Athena does [11]) to show improvement.

3. **Temporal dynamics matter** [12]: Static graphs underperform temporal graphs for change propagation. Atomic's graph evolves over time — the model should capture this.

4. **Social features are significant** [3][4]: Developer identity, team overlap, and collaboration patterns improve prediction beyond purely technical features.

5. **Evaluation protocol**: Use **filtered MRR and Hits@k** for link prediction heads [7][8], **F1/precision/recall** for conflict prediction heads, and **mRR/mAP/HIT@10** for impact analysis [11]. Consider **strat-MRR** [26] to avoid bias toward popular files.

6. **CoEdPilot's architecture** [21] is closest to what Atomic needs: multi-component system that estimates ripple effects, finds relevant prior edits, and generates edit content. Its edit-location accuracy (70.8–85.3%) and edit-dependency analysis provide strong design reference.

7. **Production-validated heuristics** (ConE [6]) provide strong non-ML baselines. Any ML system must demonstrably outperform these simpler approaches.

8. **Merge-Bench** [16][17] is the best available benchmark for conflict resolution evaluation (7,938 hunks, 11 languages, publicly available, scalable construction).

---

## Coverage Status

### Directly checked
- ✅ Merge conflict prediction methods, features, and reported metrics
- ✅ Link prediction evaluation metrics (MRR, Hits@k, AUC) and protocols
- ✅ Blast-radius / change impact analysis approaches (Athena, temporal GNN, Rex, ConE)
- ✅ Available datasets for merge conflict prediction and resolution
- ✅ Non-ML baselines for conflict detection
- ✅ Code change recommendation systems (CoEdPilot, NES, GRACE)

### Partially checked
- ⚠️ JITGNN and DeepJIT: read abstracts and key findings but not full experimental tables
- ⚠️ Temporal graph paper [12]: read abstract/summary but not full methodology

### Not checked / needs follow-up
- ❓ Specific hyperparameters and training details for Athena [11] (would need full paper read)
- ❓ ConGra [19] and AgenticFlict [20] dataset sizes and detailed schemas
- ❓ Whether striff-gnn [25] has published evaluation results (appears to be a tool, not a paper)

---

## Sources

1. Owhadi-Kareshk et al., "Predicting Merge Conflicts in Collaborative Software Development" (ESEM 2019) — https://arxiv.org/abs/1907.06274
2. Dias et al., "On the Prediction of Software Merge Conflicts" (ACM 2023) — https://dl.acm.org/doi/fullHtml/10.1145/3592813.3592931
3. Al-Refai et al., "Merge Conflict Prediction Using Feature Selection and Stacking Heterogeneous Ensembles" (JSMR 2025) — https://doi.org/10.1002/smr.70047
4. Vale et al., "Predicting merge conflicts considering social and technical assets" (EMSE 2023) — https://link.springer.com/article/10.1007/s10664-023-10395-8
5. Ziegler, "GITCoP: ML-Based Approach to Predicting Merge Conflicts" (Master's thesis) — https://www.se.cs.uni-saarland.de/theses/ThomasZieglerMA.pdf
6. Nitu et al., "ConE: A Concurrent Edit Detection Tool for Large Scale Software Development" (FSE 2021) — https://ar5iv.labs.arxiv.org/html/2101.06542
7. Berrendorf et al., "A Unified Framework for Rank-based Evaluation Metrics for Link Prediction in KGs" (2022) — https://arxiv.org/abs/2203.07544
8. Sun et al., "Revisiting the Evaluation Protocol of KG Completion Methods" (WWW 2021) — https://dl.acm.org/doi/fullHtml/10.1145/3442381.3449856
9. Bordes et al., "Translating Embeddings for Modeling Multi-relational Data" (NeurIPS 2013) — https://proceedings.neurips.cc/paper_files/paper/2013/file/1cecc7a77928ca8133fa24680a88d2f9-Paper.pdf
10. Ali et al., "Bringing Light Into the Dark: Large-scale Evaluation of KGE Models" (PyKEEN) — https://backend.orbit.dtu.dk/ws/files/262633429/Bringing_Light_Into_the_Dark_A_Large_scale_Evaluation_of_Knowledge_Graph_Embedding_Models_under_a_Unified_Framework.pdf
11. Yan et al., "Athena: Enhancing Code Understanding for Impact Analysis" (FSE 2024) — https://doi.org/10.1145/3643770
12. Zaraket et al., "Change propagation using Temporal Graphs and GNNs" (IST 2024) — https://dl.acm.org/doi/10.1016/j.infsof.2023.107368
13. Mehta et al., "Rex: Preventing Bugs via Correlated Change Analysis" (NSDI 2020) — https://www.usenix.org/system/files/nsdi20-paper-mehta.pdf
14. Hoang et al., "DeepJIT: End-to-End Deep Learning for JIT Defect Prediction" (MSR 2019) — https://doi.org/10.1109/msr.2019.00016
15. Milewicz et al., "JITGNN: Deep GNN for Just-In-Time Bug Prediction" (JSS 2024) — https://doi.org/10.1016/j.jss.2024.111984
16. Schesch et al., "Resolve Merge Conflicts with LLMs" (2025) — https://arxiv.org/html/2605.25890v1
17. Merge-Bench GitHub — https://github.com/benedikt-schesch/Merge-Bench
18. MergeConflictBench — https://github.com/Create-Inc/merge-conflict-bench
19. ConGra dataset — https://github.com/HKU-System-Security-Lab/ConGra
20. AgenticFlict dataset — https://zenodo.org/records/20118379
21. CoEdPilot (ISSTA 2024) — https://arxiv.org/html/2408.01733v1
22. NES: Next Edit Suggestion (2025) — https://arxiv.org/html/2508.02473v2
23. GRACE: Graph-Guided Repository-Aware Code Completion (2025) — https://arxiv.org/html/2509.05980v1
24. GNNContext (TSE 2025) — https://doi.org/10.1109/tse.2025.3578390
25. striff-gnn — https://github.com/hadi-technology/striff-gnn
26. Mohamed et al., "Popularity Agnostic Evaluation of KGE" (AISTATS 2020) — https://proceedings.mlr.press/v124/mohamed20a/mohamed20a.pdf
27. Inconsistency among evaluation metrics in link prediction (PMC 2024) — https://pmc.ncbi.nlm.nih.gov/articles/PMC11574622/
