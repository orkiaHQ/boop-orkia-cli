# Feuille de route Orkia — sessions agent, ChangeSets et stacks Git

> Révision produit : 1 août 2026.  
> Inspiration de référence : Atomic, source disponible sous
> `/Volumes/MyWorld/monkey.d.labs/coding/riftrHQ/_killix/inspiration_code/atomic`.  
> Direction : Git reste l’autorité du contenu et du transport ; Orkia rend
> persistantes l’intention, la causalité agent et l’orchestration des changesets.

## Décision produit

Le produit ne doit pas avoir le **diff Git** comme artefact principal. Son
unité de travail est une **session agent** qui produit des preuves et un ou
plusieurs **ChangeSets**. Les commits, branches, diffs et PRs sont les
projections Git de ces ChangeSets.

```text
Session agent signée
  └─ intention + tours + actions + lectures/écritures + validations
       └─ ChangeSet DAG stable
            ├─ projection mono-repo : branches et PRs empilées
            └─ projection multi-repo : branches, PRs et dépendances coordonnées
```

Cette direction fournit deux expériences complémentaires :

- **Stack Graphite-like** : chaque ChangeSet est projeté sur une branche Git ;
  la PR enfant cible la branche de son parent plutôt que `main`. Un changement
  d’un parent déclenche le restack déterministe des descendants.
- **Stack multi-repo** : un même DAG peut inclure des ChangeSets dans plusieurs
  dépôts. Orkia crée et actualise les branches/PRs correspondantes et expose
  leur état agrégé sans inventer un protocole de contenu concurrent à Git.

## Atomic : inspiration, pas dépendance de stockage

Atomic reste essentiel : il donne les bonnes propriétés à préserver.

| Inspiration Atomic | Traduction Orkia Git-native |
| --- | --- |
| Identités de changement durables | `ChangeSetId` stable, indépendant des hashes de commits réécrits par rebase/restack. |
| Graphe de dépendances | DAG signé de ChangeSets, parents explicites et fermeture vérifiée. |
| Concurrence déterministe | Ordre canonique des ChangeSets/opérations, règles fail-closed et tests de convergence multi-clone. |
| États vivant/supprimé/zombie | Tombstones dans les manifests sémantiques et ChangeSets supersédés, jamais effacés silencieusement. |
| Provenance complète | Liens session → action → diff → ChangeSet → commit → PR → validation. |
| Vues et isolation | Branches/worktrees Git et vues Orkia, pas un second espace de travail propriétaire. |

Orkia ne doit pas reproduire le stockage CRDT d’Atomic ni en faire une seconde
source de vérité. Les blobs, trees, commits, branches, refs, packs et remotes
Git restent autoritaires. Lorsqu’une décision sémantique ne peut pas être
prouvée, Orkia retombe explicitement sur Git normal ; il ne prétend jamais
avoir résolu une concurrence qu’il ne peut démontrer.

## Contrats fondamentaux

### Session agent

Une session possède une intention, un commit de base, des tours/actions
normalisés, les chemins observés, les commandes, les résultats, les coûts et
les attestations. Elle peut produire plusieurs ChangeSets, y compris dans des
dépôts différents.

### ChangeSet

Un ChangeSet est un objet Git immuable et signé qui contient :

- un `ChangeSetId` stable ;
- sa session d’origine et l’intention concernée ;
- le dépôt cible et le commit/base Git projeté ;
- ses parents ChangeSet et dépendances cross-repo ;
- les opérations/atomes, preuves, validations et attestations ;
- son état : proposé, actif, supersédé, intégré ou abandonné ;
- la projection courante : branche, commit et PR, qui peut être remplacée sans
  changer l’identité du ChangeSet.

Un rebase, amend ou restack change donc la projection Git mais **pas**
l’identité, l’historique de review ni la provenance du ChangeSet.

### Stack

Une stack est une vue ordonnée d’un sous-DAG de ChangeSets. Elle est :

- **mono-repo** lorsque toutes ses projections vivent dans le même dépôt ;
- **multi-repo** lorsqu’un parent ou une dépendance traverse un dépôt ;
- reproductible depuis les objets Git Orkia, même après perte de cache ou de
  service.

## Flux Graphite-like cible

```text
main
 └─ auth-model       (ChangeSet A, PR A → main)
     └─ auth-api     (ChangeSet B, PR B → auth-model)
         └─ auth-ui  (ChangeSet C, PR C → auth-api)
```

1. L’agent crée ou enrichit une session.
2. Orkia découpe les preuves et diffs observés en ChangeSets explicitement
   validés par l’utilisateur ou une règle.
3. Orkia projette chaque ChangeSet sur `orkia/cs/<id>` et crée/met à jour la PR
   avec la branche de son parent comme base.
4. Une modification de A entraîne la reprojection de A, puis le rebase/restack
   déterministe de B et C.
5. Les branches, PRs, validations et diffs changent ; les `ChangeSetId`,
   commentaires, preuves et dépendances restent stables.
6. L’intégration respecte l’ordre topologique et refuse une stack dont un
   parent est invalide, conflictuel ou insuffisamment validé.

## Flux multi-repo cible

```text
Session « OAuth »
  ├─ api/auth-model      → dépôt api, PR #120
  ├─ web/auth-client     → dépôt web, PR #88, dépend de api/auth-model
  └─ infra/auth-config   → dépôt infra, PR #341, dépend de api/auth-model
```

Chaque dépôt conserve ses commits et PRs Git ordinaires. Le DAG Orkia apporte
les dépendances qui ne peuvent pas être exprimées par une branche Git unique.
Le tableau de stack indique l’état de chaque projection et refuse l’intégration
globale tant que les préconditions inter-dépôts ne sont pas satisfaites.

## Feuille de route P0–P7 révisée

### P0 — Contrats, fixtures et migrations

- Définir les schémas versionnés `Session`, `ChangeSet`, `Stack` et
  `Projection` avec JSON canonique, signatures et migrations réversibles.
- Écrire les fixtures : restack à trois niveaux, amend d’un parent,
  abandon/supersession, dépendance cross-repo, conflits, fetch dans un ordre
  différent et perte du service/cache.
- Rendre chaque état de dégradation explicite : Git fallback, conflictuel,
  preuve manquante ou version incompatible.

**Sortie :** une suite `tests/atomic-parity/` mesure les invariants Atomic
retenus et une suite `tests/changeset-stacks/` mesure le produit réel.

### P1 — Objets Git et causalité session → ChangeSet

- Finaliser la persistance Git signée des sessions, tours, actions, intentions,
  mémoires, opérations et attestations.
- Ajouter `ChangeSet` et `Stack` sous `refs/orkia/*`, avec fermeture des
  preuves et signatures avant publication.
- Lier les diffs Git à des actions agent observées, plutôt que les traiter
  comme des artefacts autonomes.

**Sortie :** un ChangeSet portable explique exactement quelle session et quelles
preuves ont produit sa projection Git.

### P2 — Identités stables et analyse sémantique

- Stabiliser les identités Trunk/Branch/Leaf et Atom à travers renommage,
  rebase, restack et modification locale démontrable.
- Relier les opérations/atomes aux ChangeSets et aux actions agent.
- Conserver tombstones et alternatives afin qu’un ChangeSet supersédé reste
  traçable.

**Sortie :** rebase/restack ne casse ni la review ni la provenance.

### P3 — Projection de stacks mono-repo

- Créer les branches `orkia/cs/<id>` et les vues/worktrees associés.
- Projeter un DAG en branches empilées, avec la branche du parent comme base de
  PR.
- Implémenter amend, reorder, split, squash, abandon et restack automatique.
- Garder des shadow refs Git pour reconstruire les projections sans service.

**Sortie :** une stack Graphite-like est créée, modifiée et restackée
automatiquement sans changer les identités de ChangeSet.

### P4 — Merge et restack sémantique sûr

- Utiliser l’alignement token-level seulement lorsqu’il est prouvable.
- Enregistrer les conflits/résolutions comme objets signés reliés aux
  ChangeSets.
- Rebaser/restacker les descendants en ordre topologique ; arrêter la stack au
  premier conflit et fournir la provenance exacte.

**Sortie :** une édition disjointe auto-fusionne ; une concurrence ambiguë
reste un conflit Git lisible et bloquant.

### P5 — Multi-repo, sync et récupération

- Introduire un registre Git de dépôts/projections et les dépendances
  cross-repo.
- Projeter, pousser et récupérer les stacks par refs Git ordinaires.
- Reconstruire index, projections et statut de stack depuis Git.
- Finaliser OCI : layout importable, attestations, validations et digests.

**Sortie :** une session produit automatiquement une stack de PRs coordonnée
sur plusieurs dépôts ; un clone neuf peut la reconstruire.

### P6 — Review, query et expérience agent

- Produire une review par ChangeSet, avec couverture dérivée des actions
  observées et non d’un simple diff.
- Afficher session, intention, preuves, coûts, diff, parents/enfants et statut
  forge dans une même vue.
- Ajouter recherche/index reconstructible des ChangeSets, symboles et
  dépendances.

**Sortie :** un reviewer sait pourquoi un changement existe, ce qui l’a produit
et ce qui doit être intégré avant/après lui.

### P7 — Forge et plan de contrôle

- Créer/mettre à jour PRs GitHub/GitLab pour chaque projection de ChangeSet.
- Synchroniser la base des PRs lors d’un restack et préserver le lien avec les
  reviews/commentaires lorsque la forge le permet.
- Appliquer grants, équipes, expiration, révocation et rotation dans le
  serveur d’autorisation Git-native.
- Versionner les profils de dépôt/projet/workspace et négocier le protocole.

**Sortie :** Orkia orchestre une stack complète, contrôlée par la politique,
depuis une session agent jusqu’à l’intégration topologique des PRs.

## Invariants de sortie

1. **Identité durable :** un ChangeSet garde le même ID à travers restack,
   rebase et changement de commit projeté.
2. **Causalité :** toute projection est reliée à une session, à ses preuves et
   à ses validations ; le mode non capturé est explicitement faible confiance.
3. **Convergence :** des clones recevant les mêmes refs dans des ordres
   différents reconstruisent le même DAG et les mêmes projections attendues.
4. **Interop Git :** chaque branche et PR reste utilisable par Git et la forge
   sans client Orkia.
5. **Repli sûr :** sans preuve suffisante, Orkia n’auto-merge ni ne restacke ;
   il expose le conflit ou le fallback Git.
6. **Récupération :** supprimer index, cache ou service ne détruit pas les
   sessions, ChangeSets, stacks ni projections reconstructibles.
7. **Multi-repo :** une dépendance inter-dépôts est explicite et bloque
   correctement l’intégration lorsqu’un parent n’est pas prêt.

## État actuel et priorité immédiate

La base Git-native existe déjà : objets sémantiques signés, manifests,
vues/worktrees, merge prudent, capture agent, plans, vault, grants, rotation,
révocation, transport de refs, OCI initial et preuve de convergence à deux
clones.

La priorité immédiate est **P0 puis P1** : introduire le contrat `ChangeSet`
et les fixtures de stack. C’est le point qui relie les capacités déjà livrées
au besoin produit : proposer et restacker automatiquement des PRs mono-repo et
multi-repo à partir d’une session agent.
