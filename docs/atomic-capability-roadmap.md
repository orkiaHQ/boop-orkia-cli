# Feuille de route Orkia — sessions agent, stacks et ChangeSets Git-native

> Révision produit : 1 août 2026.  
> Inspiration de référence : Atomic, source disponible sous
> `/Volumes/MyWorld/monkey.d.labs/coding/riftrHQ/_killix/inspiration_code/atomic`.

## Vocabulaire canonique

- Une **StackPullRequest** est une petite PR issue d’atomes et de fragments de
  patch exacts. Elle a une seule base Git et sa propre branche de projection.
- Un **Stack** est une grosse évolution découpée en plusieurs
  StackPullRequests ordonnées dans un même dépôt.
- Un **ChangeSet** est un groupe causal de stacks ou de PRs dépendantes,
  potentiellement réparti sur plusieurs dépôts. Il coordonne publication et
  intégration ; il ne porte jamais le contenu Git.

Cette terminologie est un contrat de produit et de code. En particulier, un
commit ou un diff Git n’est ni une StackPullRequest ni un ChangeSet : c’est une
projection reproductible.

## Décision produit

Git reste l’autorité pour blobs, trees, commits, branches, refs, packs et
remotes. Orkia capture l’intention et la causalité, les signe, puis dérive les
frontières de review à l’intérieur des checkpoints.

```text
Session agent signée
  └─ intention + actions + lectures/écritures + validations
       └─ atomes et plan de review signé
            └─ StackPullRequest DAG mono-repo
                 └─ Stack ordonné
                      └─ ChangeSet multi-repo (coordination seulement)
```

Une StackPullRequest parent est projetée sur une branche Git ; l’enfant utilise
cette branche comme base. Lorsqu’un parent est révisé, les descendants sont
reprojetés dans l’ordre topologique. Les dépendances entre dépôts sont des
arêtes de ChangeSet et ne sont jamais déguisées en branche Git locale.

## Atomic : inspiration, pas second moteur Git

| Propriété d’Atomic | Traduction Orkia Git-native |
| --- | --- |
| Causalité persistante | session → action → atome → StackPullRequest → projection → PR → validation signés |
| Identité durable | les identités de StackPullRequest, Stack et ChangeSet ne dépendent pas d’un commit réécrit |
| Graphe fermé | parents locaux de StackPullRequest et dépendances de ChangeSet vérifiés avant projection ou intégration |
| Concurrence explicite | ordre déterministe, conflit ou preuve manquante bloquent ; aucune frontière n’est devinée |
| Vues isolées | worktrees, branches et refs Git, sans espace de travail propriétaire |

Orkia ne reproduit pas le stockage CRDT d’Atomic. Lorsqu’une décision ne peut
pas être démontrée à partir de la capture et des refs signées, il revient à une
review Git non scindée plutôt que de prétendre résoudre la concurrence.

## Contrats fondamentaux

### Session agent

Une session stocke intention, commit de base, actions normalisées, chemins
lus/écrits, commandes, résultats, coûts et attestations. Elle peut produire
des StackPullRequests dans plusieurs dépôts, mais une écriture inconnue réduit
la couverture et interdit l’automatisation selon la politique.

### StackPullRequest

Une StackPullRequest signée porte la session, le dépôt, les atomes fermés,
leurs preuves, validations et fragments contextuels. Sa branche, son commit et
sa PR forge sont des projections versionnées ; rebase, amend ou restack ne
changent ni son identité ni sa provenance.

### Stack

Un Stack signé référence les StackPullRequests d’un seul dépôt et leurs
racines. Il est la vue Graphite-like ordonnée, reconstruisible depuis les refs
Orkia et Git sans index local.

### ChangeSet

Un ChangeSet signé référence une ou plusieurs paires `{ dépôt, Stack }` et
éventuellement d’autres ChangeSets. Il exprime l’ordre de livraison et
d’intégration cross-repo, sans contenir patch, commit ou branche.

## Flux cible

```text
main
 └─ auth-model        (StackPullRequest A)
     └─ auth-api      (StackPullRequest B)
         └─ auth-ui   (StackPullRequest C)

Stack « OAuth » dans api : A → B → C
ChangeSet « OAuth » : Stack api + Stack web + Stack infra
```

1. L’agent enrichit une session capturée.
2. Orkia construit les atomes et le plan de review déterministe.
3. Avec couverture et confiance suffisantes, chaque unité devient une
   StackPullRequest, puis un Stack mono-repo.
4. Chaque StackPullRequest est projetée sur `orkia/stack-pr/<id>` ; une PR
   enfant cible la branche de son parent.
5. Les stacks de dépôts distincts sont reliés par un ChangeSet et restent
   bloqués tant qu’une dépendance externe n’est pas publiée et vérifiée.
6. Une révision amont reprojette les descendants, sans modifier les identités
   causales ; le ChangeSet conserve l’orchestration multi-repo.

## Feuille de route de parité Atomic

### P0 — Contrats et preuves

- Schémas versionnés, JSON canonique, signatures et migrations pour Session,
  StackPullRequest, Stack, ChangeSet et Projection.
- Fixtures de restack, amend, supersession, split, dépendance cross-repo,
  conflit, fetch dans un ordre différent et perte de cache.
- États fail-closed explicites : preuve manquante, conflit, politique refusée
  ou version incompatible.

### P1 — Causalité session → StackPullRequest

- Persistance Git signée de sessions, tours, actions, intentions, mémoires,
  opérations et attestations.
- Fermeture des preuves et des fragments de patch avant publication d’une
  StackPullRequest.
- Lien des diffs observés aux actions d’agent, jamais au seul commit final.

### P2 — Analyse et identités stables

- Stabilité des identités Trunk/Branch/Leaf et Atom à travers renommage,
  rebase, restack et modification locale prouvable.
- Tombstones et alternatives pour qu’une StackPullRequest supersédée demeure
  traçable.

### P3 — Stacks mono-repo

- Branches `orkia/stack-pr/<id>`, worktrees isolés et PRs empilées.
- Restack topologique, reorder, split, fusion, abandon et PR de fermeture
  explicite lorsque le graphe a plusieurs parents.
- Shadow refs reconstructibles sans serveur.

### P4 — Application sémantique sûre

- Alignement symboles/blocs/token seulement lorsque la preuve est unique.
- Conflits et résolutions signés, reliés aux StackPullRequests.
- Arrêt de la stack au premier conflit, avec provenance lisible.

### P5 — ChangeSets multi-repo

- Registre de dépôts, vérification de chaque Stack référencé et dépendances
  cross-repo explicites.
- Projection, push et récupération par refs Git ordinaires.
- Reconstruction de l’index, des projections et du statut depuis Git.

### P6 — Review et expérience agent

- Review par StackPullRequest avec couverture dérivée de la capture.
- Vue unifiée session, intention, preuves, coûts, diff, parents/enfants et
  statut forge.
- Recherche/index reconstruisibles des StackPullRequests, Stacks et
  ChangeSets.

### P7 — Forge et contrôle d’intégration

- Création ou mise à jour GitHub/GitLab de chaque StackPullRequest projetée.
- Synchronisation des bases lors d’un restack et préservation des liens de
  review lorsque la forge le permet.
- Politique, grants, équipes, expiration, révocation et rotation côté serveur.

## Invariants de sortie

1. Une StackPullRequest conserve son ID à travers rebase et restack ; un Stack
   et un ChangeSet conservent le leur à travers les projections.
2. Toute projection est reliée à une session, ses preuves et ses validations.
3. Des clones recevant les mêmes refs dans des ordres différents reconstruisent
   les mêmes objets et le même ordre déterministe.
4. Les branches et PRs restent utilisables par Git et la forge sans client
   Orkia.
5. Sans preuve suffisante, Orkia ne crée pas de stack automatique et ne
   déclenche pas d’intégration automatique.
6. La perte d’un index, cache ou serveur ne détruit aucun objet reconstructible.
7. Une dépendance inter-dépôts bloque l’intégration tant que le Stack requis
   n’est pas publié et vérifié.
