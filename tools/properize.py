#!/usr/bin/env python
import argparse
import json
from collections import defaultdict

import networkx as nx
from tokenizers import Tokenizer


def bpe_naive(rules, seq, return_seq=False):
    last_rank, largest_rank = None, None

    def update_answer(rank):
        nonlocal last_rank, largest_rank
        last_rank = rank
        if largest_rank is None or rank > largest_rank:
            largest_rank = rank

    def find_merge():
        nonlocal rules, seq
        best_rank, best_pos, result_token = None, None, None
        for i in range(1, len(seq)):
            rank, token = rules.get((seq[i - 1], seq[i]), (None, None))
            if rank is None:
                continue
            if best_rank is None or rank < best_rank:
                best_rank, best_pos, result_token = rank, i, token
        return best_rank, best_pos, result_token

    def apply_merge(pos, token):
        nonlocal seq
        seq.pop(pos)
        seq[pos - 1] = token

    while res := find_merge():
        rank, pos, token = res
        if rank is None:
            break
        update_answer(rank)
        apply_merge(pos, token)

    if return_seq:
        return seq
    return len(seq) == 1, last_rank, largest_rank


def rules_from_merges(vocab, merges):
    return {(vocab[u], vocab[v]): (k, vocab[u + v]) for k, (u, v) in enumerate(merges)}


def load_hf_tokenizer(path):
    tokenizer = Tokenizer.from_file(path)
    data = json.loads(tokenizer.to_str())
    return data


def to_seq(vocab, text):
    return [vocab[i] for i in text]


def build_growing_trees(rules, vocab):
    vocab_r = {v: k for k, v in vocab.items()}
    rank_to_pre_suc = dict()
    for (u, v), (rank_id, _) in rules.items():
        rank_to_pre_suc[rank_id] = u, v

    nodes, parents, children = dict(), dict(), defaultdict(list)

    for token in vocab:
        single, last_rank, large_rank = bpe_naive(rules, to_seq(vocab, token))

        if not single or last_rank is None:
            continue

        pre, suc = rank_to_pre_suc[last_rank]

        if last_rank == large_rank:
            nodes[last_rank] = "L", pre
            continue

        pre_res = bpe_naive(rules, to_seq(vocab, vocab_r[pre]))
        pre_single, pre_last_rank, pre_large_rank = pre_res

        suc_res = bpe_naive(rules, to_seq(vocab, vocab_r[suc]))
        suc_single, suc_last_rank, suc_large_rank = suc_res

        assert pre_single and suc_single

        if suc_large_rank == large_rank:
            nodes[last_rank] = "L", pre
            parents[last_rank] = suc_last_rank
        else:
            assert pre_large_rank == large_rank
            nodes[last_rank] = "R", suc
            parents[last_rank] = pre_last_rank

        children[parents[last_rank]].append(last_rank)

    for k in nodes:
        children[k].sort()

    children = {k: v for k, v in children.items()}

    return nodes, parents, children


def build_dep_graphs(nodes, parents, children):
    for root in sorted(k for k in nodes if k not in parents):
        stack, token_dep, edges = [root], defaultdict(list), []

        while stack:
            node = stack.pop()
            token_dep[nodes[node]].append(node)
            prev = node
            for child in children[node]:
                stack.append(child)
                edges.append((prev, child))
                prev = child

        if len(token_dep) <= 1:
            yield root, ()
            continue

        for direction, token_id in token_dep:
            if direction != "L":
                continue
            for u in token_dep.get(("R", token_id), ()):
                for v in token_dep.get(("L", token_id), ()):
                    edges.append((u, v))

        yield root, edges


def properize(rules, vocab):
    nodes, parents, children = build_growing_trees(rules, vocab)
    assert len(nodes) < len(vocab)

    order, last_root = [], None
    for root, edges in build_dep_graphs(nodes, parents, children):
        if last_root is not None:
            assert last_root < root
        last_root = root

        if not edges:
            order.append(root)
            continue

        g = nx.DiGraph()
        g.add_edges_from(edges)
        try:
            topo_order = list(nx.lexicographical_topological_sort(g))
        except nx.NetworkXUnfeasible:
            print(f"loop is found for the rule with id {root}, failed to properize")
            print(json.dumps(edges))
            try:
                import matplotlib.pyplot as plt

                nx.draw(g, with_labels=True)
                plt.show()
            except Exception:
                print("failed to display graph")
                pass
            raise

        assert topo_order[0] == root
        assert all(topo_order[i] < root for i in range(1, len(topo_order)))

        order.extend(topo_order)

    assert len(order) == len(nodes) == len(frozenset(order))

    return order


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("input")
    parser.add_argument("output")
    args = parser.parse_args()

    data = load_hf_tokenizer(args.input)
    vocab = data["model"]["vocab"]
    merges = data["model"]["merges"]
    rules = rules_from_merges(vocab, merges)

    order = properize(rules, vocab)
    new_merges = [merges[i] for i in order]
    new_rules = rules_from_merges(vocab, new_merges)

    for token in vocab:
        old_single, old_rank, _ = bpe_naive(rules, to_seq(vocab, token))
        new_single, new_rank, large_rank = bpe_naive(new_rules, to_seq(vocab, token))

        assert old_single == new_single
        assert new_rank == large_rank

        if new_single:
            assert (old_rank is None and new_rank is None) or (
                old_rank is not None
                and new_rank is not None
                and merges[old_rank] == new_merges[new_rank]
            )

        original = bpe_naive(rules, to_seq(vocab, token), return_seq=True)
        properized = bpe_naive(new_rules, to_seq(vocab, token), return_seq=True)
        assert original == properized, token

    data["model"]["merges"] = new_merges
    with open(args.output, "w") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)


if __name__ == "__main__":
    main()
