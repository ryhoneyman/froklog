"""model.py — Conditional Transformer decoder for NPC chat generation.

Architecture summary:
  - 4 decoder layers, 4 attention heads, d_model=128, d_ff=512
  - Causal self-attention (GPT-style — no cross-attention needed)
  - Positional embeddings learnt, not sinusoidal
  - Weight tying between input embeddings and output projection
  - ~4 M parameters at default settings → fast CPU inference

Conditioning:
  The personality/state/trigger context is encoded as a sequence of control
  tokens prepended to the response (see build_dataset.py).  The model sees
  them as ordinary tokens and learns to condition its response distribution
  on them through causal attention.

ONNX export:
  Call ChatDecoder.forward(tokens) with tokens shape (1, seq_len).
  For autoregressive decoding, build a growing token tensor and call the
  model repeatedly, sampling from logits[:, -1, :] at each step.
"""

import math

import torch
import torch.nn as nn
import torch.nn.functional as F


# ── Attention ─────────────────────────────────────────────────────────────────

class CausalSelfAttention(nn.Module):
    def __init__(self, d_model: int, n_heads: int, dropout: float = 0.1) -> None:
        super().__init__()
        assert d_model % n_heads == 0, "d_model must be divisible by n_heads"
        self.n_heads = n_heads
        self.d_head = d_model // n_heads
        self.qkv = nn.Linear(d_model, 3 * d_model, bias=False)
        self.out = nn.Linear(d_model, d_model, bias=False)
        self.drop = nn.Dropout(dropout)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        B, T, C = x.shape
        # Project to Q, K, V and reshape to (B, n_heads, T, d_head)
        qkv = self.qkv(x).reshape(B, T, 3, self.n_heads, self.d_head)
        qkv = qkv.permute(2, 0, 3, 1, 4)   # (3, B, H, T, d_head)
        q, k, v = qkv[0], qkv[1], qkv[2]

        scale = math.sqrt(self.d_head)
        attn = (q @ k.transpose(-2, -1)) / scale

        # Causal mask: upper triangle → -inf so future tokens are invisible
        causal_mask = torch.triu(
            torch.ones(T, T, device=x.device, dtype=torch.bool), diagonal=1
        )
        attn = attn.masked_fill(causal_mask, float("-inf"))
        attn = F.softmax(attn, dim=-1)
        attn = self.drop(attn)

        out = (attn @ v).transpose(1, 2).reshape(B, T, C)
        return self.out(out)


# ── Transformer block ─────────────────────────────────────────────────────────

class TransformerBlock(nn.Module):
    def __init__(
        self, d_model: int, n_heads: int, d_ff: int, dropout: float = 0.1
    ) -> None:
        super().__init__()
        self.ln1 = nn.LayerNorm(d_model)
        self.attn = CausalSelfAttention(d_model, n_heads, dropout)
        self.ln2 = nn.LayerNorm(d_model)
        self.ff = nn.Sequential(
            nn.Linear(d_model, d_ff),
            nn.GELU(),
            nn.Linear(d_ff, d_model),
            nn.Dropout(dropout),
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        x = x + self.attn(self.ln1(x))
        x = x + self.ff(self.ln2(x))
        return x


# ── Main model ────────────────────────────────────────────────────────────────

class ChatDecoder(nn.Module):
    """Conditional causal Transformer decoder for NPC chat generation.

    Input  tokens  : (B, T)  int64
    Output logits  : (B, T, vocab_size)  float32

    Training loss is computed only on response-token positions (positions at or
    after the [RESPONSE] control token).  The caller (train.py) builds the mask
    from the prefix_lens array produced by build_dataset.py.
    """

    def __init__(
        self,
        vocab_size: int,
        d_model: int = 128,
        n_layers: int = 4,
        n_heads: int = 4,
        d_ff: int = 512,
        max_len: int = 128,
        dropout: float = 0.1,
        pad_id: int = 0,
    ) -> None:
        super().__init__()
        self.pad_id = pad_id
        self.d_model = d_model
        self.vocab_size = vocab_size

        self.tok_emb = nn.Embedding(vocab_size, d_model, padding_idx=pad_id)
        self.pos_emb = nn.Embedding(max_len, d_model)
        self.drop = nn.Dropout(dropout)
        # Constant position-index buffer: avoids a dynamic Range op on ONNX export
        # so that tract-onnx (which lacks Range) can run the model via Slice instead.
        self.register_buffer(
            "position_ids", torch.arange(max_len).unsqueeze(0), persistent=False
        )

        self.blocks = nn.ModuleList(
            [TransformerBlock(d_model, n_heads, d_ff, dropout) for _ in range(n_layers)]
        )
        self.ln_f = nn.LayerNorm(d_model)
        self.head = nn.Linear(d_model, vocab_size, bias=False)

        # Weight tying: the output projection shares weights with the token embeddings.
        # This reduces parameter count and improves generalisation on small corpora.
        self.head.weight = self.tok_emb.weight

        self._init_weights()

    def _init_weights(self) -> None:
        for m in self.modules():
            if isinstance(m, nn.Linear):
                nn.init.normal_(m.weight, std=0.02)
                if m.bias is not None:
                    nn.init.zeros_(m.bias)
            elif isinstance(m, nn.Embedding):
                nn.init.normal_(m.weight, std=0.02)
            elif isinstance(m, nn.LayerNorm):
                nn.init.ones_(m.weight)
                nn.init.zeros_(m.bias)

    def forward(self, tokens: torch.Tensor) -> torch.Tensor:
        """tokens: (B, T) int64 → logits: (B, T, vocab_size)"""
        B, T = tokens.shape
        pos = self.position_ids[:, :T]   # (1, T) — Slice op, not Range
        x = self.drop(self.tok_emb(tokens) + self.pos_emb(pos))
        for block in self.blocks:
            x = block(x)
        x = self.ln_f(x)
        return self.head(x)

    @torch.no_grad()
    def generate(
        self,
        prefix: torch.Tensor,
        max_new_tokens: int = 64,
        temperature: float = 0.9,
        top_k: int = 40,
        eos_id: int = 2,
    ) -> list[int]:
        """Autoregressive generation given a prefix tensor of shape (1, T).

        Returns the list of newly generated token IDs (not including the prefix).
        Stops at EOS or max_new_tokens, whichever comes first.
        """
        tokens = prefix.clone()
        for _ in range(max_new_tokens):
            logits = self(tokens)[:, -1, :]     # (1, vocab)
            logits = logits / max(temperature, 1e-6)

            if top_k > 0:
                topk_vals, _ = torch.topk(logits, min(top_k, logits.size(-1)))
                logits[logits < topk_vals[:, -1:]] = float("-inf")

            probs = F.softmax(logits, dim=-1)
            next_tok = torch.multinomial(probs, num_samples=1)     # (1, 1)
            tokens = torch.cat([tokens, next_tok], dim=1)

            if next_tok.item() == eos_id:
                break

        return tokens[0, prefix.shape[1]:].tolist()


# ── Factory ───────────────────────────────────────────────────────────────────

def model_from_meta(meta: dict) -> ChatDecoder:
    """Construct a model from the dataset meta.json dict."""
    return ChatDecoder(
        vocab_size=meta["vocab_size"],
        max_len=meta["max_len"],
        pad_id=meta["pad_id"],
    )
