// Command gen writes the C2SP interop vectors the crate's tests read
// (tests/vectors/c2sp/), from fixed seeds, with the Go reference
// implementations: golang.org/x/mod/sumdb/{note,tlog},
// github.com/transparency-dev/{formats,merkle} and the stock
// github.com/transparency-dev/witness run in process. Every run writes the
// same bytes.
//
//	go run ./gen [-out ../../tests/vectors/c2sp]
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"flag"
	"fmt"
	"strings"

	"github.com/hupe1980/doubleentry/interop/c2sp/internal/vec"
	f_log "github.com/transparency-dev/formats/log"
	f_note "github.com/transparency-dev/formats/note"
	"github.com/transparency-dev/formats/policy"
	"github.com/transparency-dev/formats/proof"
	m_proof "github.com/transparency-dev/merkle/proof"
	"github.com/transparency-dev/merkle/rfc6962"
	"golang.org/x/mod/sumdb/note"
	"golang.org/x/mod/sumdb/tlog"
)

const (
	origin = "example.com/ledgers/acme"
	// The fixed cosignature timestamp (2026-10-16T00:00:00Z).
	cosignTime = 1792108800
	bigSize    = 70000
)

var (
	logKey = vec.LogSigner{KeyName: origin, Key: vec.Ed25519(7)}
	w1     = &vec.Cosigner{KeyName: "witness.example/w1", Key: vec.Ed25519(9), Time: cosignTime}
	w2     = &vec.Cosigner{KeyName: "witness.example/w2", Key: vec.Ed25519(10), Time: cosignTime}
	w3     = &vec.Cosigner{KeyName: "witness.example/w3", Key: vec.Ed25519(11), Time: cosignTime}
	other  = vec.LogSigner{KeyName: origin, Key: vec.Ed25519(8)}
	ml     = vec.NewMLDSACosigner("witness.example/ml", 12, cosignTime)
	out    *vec.Writer
	tree   = newTree(bigSize)
)

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func verdict(err error) string {
	if err != nil {
		return "reject"
	}
	return "accept"
}

func b64(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// ── The RFC 6962 tree of leaves "leaf-i" ────────────────────────────────

type hashTree struct {
	stored []tlog.Hash
}

func newTree(n int64) *hashTree {
	t := &hashTree{}
	for i := int64(0); i < n; i++ {
		hashes := must(tlog.StoredHashes(i, []byte(fmt.Sprintf("leaf-%d", i)), t))
		t.stored = append(t.stored, hashes...)
	}
	return t
}

func (t *hashTree) ReadHashes(indexes []int64) ([]tlog.Hash, error) {
	out := make([]tlog.Hash, len(indexes))
	for i, x := range indexes {
		out[i] = t.stored[x]
	}
	return out, nil
}

func (t *hashTree) root(n int64) []byte {
	h := must(tlog.TreeHash(n, t))
	return h[:]
}

func (t *hashTree) inclusion(index, n int64) [][]byte {
	p := must(tlog.ProveRecord(n, index, t))
	leaf := tlog.RecordHash([]byte(fmt.Sprintf("leaf-%d", index)))
	path := hashes(p)
	check(m_proof.VerifyInclusion(rfc6962.DefaultHasher, uint64(index), uint64(n), leaf[:], path, t.root(n)))
	return path
}

func (t *hashTree) consistency(old, n int64) [][]byte {
	p := must(tlog.ProveTree(n, old, t))
	path := hashes(p)
	check(m_proof.VerifyConsistency(rfc6962.DefaultHasher, uint64(old), uint64(n), path, t.root(old), t.root(n)))
	return path
}

func hashes(in []tlog.Hash) [][]byte {
	out := make([][]byte, len(in))
	for i := range in {
		h := in[i]
		out[i] = h[:]
	}
	return out
}

func joined(path [][]byte) string {
	parts := make([]string, len(path))
	for i, h := range path {
		parts[i] = b64(h)
	}
	return strings.Join(parts, " ")
}

// ── Checkpoints ──────────────────────────────────────────────────────────

func text(size int64, root []byte, extensions ...string) string {
	t := fmt.Sprintf("%s\n%d\n%s\n", origin, size, b64(root))
	for _, e := range extensions {
		t += e + "\n"
	}
	return t
}

func sign(text string, signers ...note.Signer) []byte {
	return must(note.Sign(&note.Note{Text: text}, signers...))
}

// cosign adds cosignatures to a signed note, as a witness would.
func cosign(signed []byte, signers ...note.Signer) []byte {
	n := must(note.Open(signed, note.VerifierList(must(note.NewVerifier(logKey.VKey())))))
	return must(note.Sign(n, signers...))
}

var (
	empty   = rfc6962.DefaultHasher.EmptyRoot()
	cp0     = sign(text(0, empty), logKey)
	cp1     = sign(text(1, tree.root(1)), logKey)
	cp7     = sign(text(7, tree.root(7)), logKey)
	cp13    = sign(text(13, tree.root(13)), logKey)
	cp13ext = sign(text(13, tree.root(13), "ext one", "ext two"), logKey)
	cpBig   = sign(text(bigSize, tree.root(bigSize)), logKey)
)

func parseCheckpoint(signed []byte) error {
	_, _, _, err := f_log.ParseCheckpoint(signed, origin, must(note.NewVerifier(logKey.VKey())))
	return err
}

func notes() {
	for _, c := range []struct {
		name string
		size int64
		note []byte
		root []byte
	}{
		{"size-0", 0, cp0, empty},
		{"size-1", 1, cp1, tree.root(1)},
		{"size-13", 13, cp13, tree.root(13)},
		{"size-70000", bigSize, cpBig, tree.root(bigSize)},
	} {
		v := &vec.Vector{Comment: "a checkpoint signed by note.Sign with the 0x01 log key from seed [7; 32]"}
		v.In("seed", "07").In("origin", origin).In("size", c.size).In("root", b64(c.root))
		v.Out("note_b64", c.note).Out("vkey", logKey.VKey()).Out("go", verdict(parseCheckpoint(c.note)))
		out.Write("notes/checkpoint-"+c.name+".txt", v)
	}
	v := &vec.Vector{Comment: "a checkpoint with two extension lines, signed by note.Sign; kept verbatim"}
	v.In("note_b64", cp13ext).In("vkey", logKey.VKey())
	v.Out("go", verdict(parseCheckpoint(cp13ext))).Out("size", 13).Out("extensions", 2)
	out.Write("notes/checkpoint-extensions.txt", v)

	// The example of c2sp.org/signed-note.
	example := "This is an example message.\n\n— example.com/foo Uw2QOkn8srV1yJGh2VYRlL1Tnagv1YEq6TfXppzi2ONncAlTgK7Ztg1ERYNZXsYjOBH3mFXmRKuwHjG1Yu72IneyaQM=\n"
	exampleKey := "example.com/foo+530d903a+AekyeRrm56hApGFkyQR4ZCbV54Id2LKaANYcrnKv3U2k"
	_, err := note.Open([]byte(example), note.VerifierList(must(note.NewVerifier(exampleKey))))
	v = &vec.Vector{Comment: "the example of c2sp.org/signed-note, verified by note.Open"}
	v.In("note_b64", []byte(example)).In("vkey", exampleKey).Out("go", verdict(err))
	out.Write("notes/signed-note-example.txt", v)

	malformed()
}

// malformed writes notes that break one rule each, with Go's verdict
// (formats/log.ParseCheckpoint with the log key) and the crate's.
func malformed() {
	sig := strings.SplitN(string(cp13), "\n\n", 2)[1]
	body := text(13, tree.root(13))
	unknown := func(i int) string {
		s := vec.LogSigner{KeyName: fmt.Sprintf("unknown.example/%d", i), Key: vec.Ed25519(byte(100 + i))}
		raw := must(note.Sign(&note.Note{Text: body}, s))
		return strings.SplitN(string(raw), "\n\n", 2)[1]
	}
	many := func(n int) string {
		var b strings.Builder
		b.WriteString(sig)
		for i := 0; i < n; i++ {
			b.WriteString(unknown(i))
		}
		return b.String()
	}
	// A canonical signature line with non-zero padding bits: the same bytes
	// under a second text (Go's lax decoder accepts it).
	line := strings.TrimSuffix(sig, "\n")
	noncanonical := line[:len(line)-2] + string(line[len(line)-2]+1) + "="
	shortSig := "— " + origin + " " + b64([]byte{1, 2, 3, 4}) + "\n"
	cases := []struct {
		name, crate string
		note        []byte
	}{
		{"missing-final-newline", "Malformed:MissingNewline", []byte(strings.TrimSuffix(string(cp13), "\n"))},
		{"blank-line-after-signatures", "Malformed:NoSignature", append(append([]byte{}, cp13...), '\n')},
		{"no-separator", "Malformed:MissingSeparator", []byte(strings.Replace(string(cp13), "\n\n", "\n", 1))},
		{"zero-signatures", "Malformed:NoSignature", []byte(body + "\n")},
		{"invalid-utf8", "Malformed:Utf8", []byte(strings.Replace(string(cp13), "ledgers", "ledg\xffrs", 1))},
		{"control-character", "Malformed:ControlChar", []byte(strings.Replace(string(cp13), "ledgers", "ledg\trs", 1))},
		{"non-canonical-base64", "Malformed:Base64", []byte(body + "\n" + noncanonical + "\n")},
		{"no-em-dash", "Malformed:SignatureLine", []byte(body + "\n" + strings.Replace(sig, "—", "-", 1))},
		{"short-signature", "Malformed:SignatureLine", []byte(body + "\n" + shortSig)},
		{"duplicate-signature", "DuplicateSignature", []byte(body + "\n" + sig + sig)},
		{"101-signatures", "TooManySignatures", []byte(body + "\n" + many(100))},
		{"16-signatures", "accept", []byte(body + "\n" + many(15))},
		{"size-leading-zero", "Malformed:Decimal", sign(strings.Replace(body, "\n13\n", "\n013\n", 1), logKey)},
		{"root-31-bytes", "Malformed:RootLength", sign(text(13, tree.root(13)[:31]), logKey)},
		{"empty-extension-line", "Malformed:EmptyLine", sign(body+"\next\n", logKey)},
		{"name-with-plus", "BadName", []byte(body + "\n" + strings.Replace(sig, origin, "a+b", 1))},
	}
	for _, c := range cases {
		v := &vec.Vector{Comment: "a checkpoint note that breaks one rule: " + c.name}
		v.In("note_b64", c.note).In("vkey", logKey.VKey())
		v.Out("go", verdict(parseCheckpoint(c.note))).Out("crate", c.crate)
		out.Write("notes/malformed/"+c.name+".txt", v)
	}
}

// ── Keys ─────────────────────────────────────────────────────────────────

func keys() {
	logTyped := must(base64.StdEncoding.DecodeString(strings.SplitN(logKey.VKey(), "+", 3)[2]))
	withType := func(t byte, key []byte) []byte { return append([]byte{t}, key...) }
	smallOrder := make([]byte, 32)
	smallOrder[0] = 1 // the identity point: order 1
	long := strings.Repeat("n", 255)
	cases := []struct {
		name, vkey, crate string
		goErr             error
	}{
		{"log-ed25519", logKey.VKey(), "Ed25519", nil},
		{"witness-cosignature", w1.VKey(), "CosignatureEd25519", nil},
		{"signed-note-example", "example.com/foo+530d903a+AekyeRrm56hApGFkyQR4ZCbV54Id2LKaANYcrnKv3U2k", "Ed25519", nil},
		{"ml-dsa-44", ml.VKey(), "UnsupportedKeyType:6", nil},
		{"bad-key-id", strings.Replace(logKey.VKey(), "+c3b6d994+", "+c3b6d995+", 1), "KeyIdMismatch", nil},
		{"uppercase-key-id", strings.Replace(logKey.VKey(), "+c3b6d994+", "+C3B6D994+", 1), "Malformed:Vkey", nil},
		{"small-order", vec.VKey(origin, withType(0x01, smallOrder)), "WeakKey", nil},
		{"type-02", vec.VKey(origin, withType(0x02, logTyped[1:])), "UnsupportedKeyType:2", nil},
		{"type-05", vec.VKey(origin, withType(0x05, logTyped[1:])), "UnsupportedKeyType:5", nil},
		{"type-ff", vec.VKey(origin, withType(0xff, logTyped[1:])), "UnsupportedKeyType:255", nil},
		{"short-key", vec.VKey(origin, logTyped[:32]), "Malformed:Vkey", nil},
		{"name-255-bytes", vec.VKey(long, logTyped), "Ed25519", nil},
		{"name-256-bytes", vec.VKey(long+"n", logTyped), "BadName", nil},
	}
	for _, c := range cases {
		var err error
		if strings.Contains(c.name, "witness") || c.name == "ml-dsa-44" {
			_, err = f_note.NewVerifierForCosignatureV1(c.vkey)
		} else {
			_, err = note.NewVerifier(c.vkey)
		}
		v := &vec.Vector{Comment: "a verifier key: " + c.name}
		v.In("vkey", c.vkey).Out("go", verdict(err)).Out("crate", c.crate)
		out.Write("keys/"+c.name+".txt", v)
	}
}

// ── Trees ────────────────────────────────────────────────────────────────

func trees() {
	roots := &vec.Vector{Comment: "RFC 6962 roots of the trees of leaves leaf-0, leaf-1, … (tlog.TreeHash)"}
	roots.In("leaves", "leaf-i")
	for n := int64(0); n <= 64; n++ {
		roots.Out("root", fmt.Sprintf("%d %s", n, b64(tree.root(n))))
	}
	roots.Out("root", fmt.Sprintf("%d %s", bigSize, b64(tree.root(bigSize))))
	out.Write("trees/roots.txt", roots)

	inclusion := &vec.Vector{Comment: "inclusion proofs (tlog.ProveRecord, checked by merkle/proof.VerifyInclusion): index size path…"}
	inclusion.In("leaves", "leaf-i")
	for n := int64(1); n <= 64; n++ {
		for i := int64(0); i < n; i++ {
			inclusion.Out("proof", strings.TrimSpace(fmt.Sprintf("%d %d %s", i, n, joined(tree.inclusion(i, n)))))
		}
	}
	for _, i := range []int64{0, 5, 65535, 69999} {
		inclusion.Out("proof", fmt.Sprintf("%d %d %s", i, bigSize, joined(tree.inclusion(i, bigSize))))
	}
	out.Write("trees/inclusion.txt", inclusion)

	consistency := &vec.Vector{Comment: "consistency proofs (tlog.ProveTree, checked by merkle/proof.VerifyConsistency): old new path…"}
	consistency.In("leaves", "leaf-i")
	for n := int64(1); n <= 64; n++ {
		for m := int64(1); m <= n; m++ {
			consistency.Out("proof", strings.TrimSpace(fmt.Sprintf("%d %d %s", m, n, joined(tree.consistency(m, n)))))
		}
	}
	for _, m := range []int64{7, 65536, 69999} {
		consistency.Out("proof", fmt.Sprintf("%d %d %s", m, bigSize, joined(tree.consistency(m, bigSize))))
	}
	out.Write("trees/consistency.txt", consistency)
}

// ── Witness protocol ─────────────────────────────────────────────────────

func request(old int64, signed []byte, path [][]byte) []byte {
	var b bytes.Buffer
	fmt.Fprintf(&b, "old %d\n", old)
	for _, h := range path {
		fmt.Fprintf(&b, "%s\n", b64(h))
	}
	b.WriteString("\n")
	b.Write(signed)
	return b.Bytes()
}

func witnessVectors() {
	// Six request bodies, each accepted by a fresh stock witness that first
	// holds `old` (reached with an old-0 request at that size).
	at := map[int64][]byte{0: cp0, 1: cp1, 7: cp7, 13: cp13}
	for _, c := range []struct {
		name     string
		old      int64
		signed   []byte
		size     int64
		extended bool
	}{
		{"old-0-to-1", 0, cp1, 1, false},
		{"old-0-to-13", 0, cp13, 13, false},
		{"old-1-to-13", 1, cp13, 13, false},
		{"old-7-to-13", 7, cp13, 13, false},
		{"old-13-to-13", 13, cp13, 13, false},
		{"old-7-to-13-extensions", 7, cp13ext, 13, true},
	} {
		var path [][]byte
		if c.old != 0 && c.old != c.size {
			path = tree.consistency(c.old, c.size)
		}
		body := request(c.old, c.signed, path)
		w := vec.NewWitness(w1, logKey.VKey())
		if c.old > 0 {
			if r := w.Post(request(0, at[c.old], nil)); r.Status != 200 {
				panic(fmt.Sprintf("seeding %s: %d", c.name, r.Status))
			}
		}
		reply := w.Post(body)
		w.Close()
		v := &vec.Vector{Comment: "an add-checkpoint body (tlog-witness), posted to the stock witness holding `old`"}
		v.In("checkpoint_b64", c.signed).In("old", c.old).In("proof", joined(path))
		v.Out("body_b64", body).Out("status", reply.Status).Out("reply_b64", reply.Body)
		out.Write("witness/requests/"+c.name+".txt", v)
	}

	// Replies captured from the stock witness.
	type captured struct {
		name    string
		outcome string
		setup   func(w *vec.Witness)
		body    []byte
	}
	foreign := sign(text(13, tree.root(13)), other)
	unknownOrigin := sign(strings.Replace(text(13, tree.root(13)), origin, "unknown.example/log", 1),
		vec.LogSigner{KeyName: "unknown.example/log", Key: vec.Ed25519(8)})
	badPath := tree.consistency(7, 13)
	badPath[0] = append([]byte{}, badPath[0]...)
	badPath[0][0] ^= 1
	otherRoot := sign(text(13, tree.root(12)), logKey)
	seed7 := func(w *vec.Witness) { w.Post(request(0, cp7, nil)) }
	seed13 := func(w *vec.Witness) { w.Post(request(0, cp13, nil)) }
	for _, c := range []captured{
		{"200", "Cosigned", nil, request(0, cp13, nil)},
		{"409", "Conflict:7", seed7, request(0, cp13, nil)},
		{"400", "BadRequest", seed13, request(14, cp13, nil)},
		{"403", "Forbidden", nil, request(0, foreign, nil)},
		{"404", "UnknownLog", nil, request(0, unknownOrigin, nil)},
		{"422-proof", "Unprocessable", seed7, request(7, cp13, badPath)},
		{"422-root", "Unprocessable", seed13, request(13, otherRoot, nil)},
	} {
		w := vec.NewWitness(w1, logKey.VKey())
		if c.setup != nil {
			c.setup(w)
		}
		r := w.Post(c.body)
		w.Close()
		v := &vec.Vector{Comment: "a reply of the stock witness to an add-checkpoint request"}
		v.In("request_b64", c.body).In("witness", w1.VKey())
		v.In("status", r.Status).In("content_type", r.ContentType).In("body_b64", r.Body)
		v.Out("crate", c.outcome)
		out.Write("witness/replies/"+c.name+".txt", v)
	}
	// Replies the stock witness does not send but the protocol allows.
	for _, c := range []struct {
		name, contentType, outcome string
		status                     int
		body                       []byte
	}{
		{"429", "", "Pushback", 429, nil},
		{"500", "", "Unexpected:500", 500, nil},
		{"409-no-media-type", "", "MalformedReply", 409, []byte("9\n")},
		{"409-leading-zero", "text/x.tlog.size", "MalformedReply", 409, []byte("09\n")},
		{"409-no-newline", "text/x.tlog.size", "MalformedReply", 409, []byte("9")},
		{"409-two-newlines", "text/x.tlog.size", "MalformedReply", 409, []byte("9\n\n")},
		{"409-negative", "text/x.tlog.size", "MalformedReply", 409, []byte("-1\n")},
		{"409-media-type-case-and-parameters", "Text/X.TLOG.SIZE; charset=utf-8", "Conflict:9", 409, []byte("9\n")},
	} {
		v := &vec.Vector{Comment: "a synthesised witness reply"}
		v.In("request_b64", request(0, cp13, nil)).In("witness", w1.VKey())
		v.In("status", c.status).In("content_type", c.contentType).In("body_b64", c.body)
		v.Out("crate", c.outcome)
		out.Write("witness/replies/"+c.name+".txt", v)
	}

	// Four cosignatures: two witness keys × the plain and the extended
	// checkpoint, each checked by formats' cosignature verifier.
	for _, w := range []*vec.Cosigner{w1, w2} {
		for _, c := range []struct {
			name   string
			signed []byte
		}{{"plain", cp13}, {"extensions", cp13ext}} {
			cosigned := cosign(c.signed, w)
			wv := must(f_note.NewVerifierForCosignatureV1(w.VKey()))
			lv := must(note.NewVerifier(logKey.VKey()))
			n, err := note.Open(cosigned, note.VerifierList(lv, wv))
			check(err)
			if len(n.Sigs) != 2 {
				panic("cosignature not verified")
			}
			line := "— " + n.Sigs[1].Name + " " + n.Sigs[1].Base64
			v := &vec.Vector{Comment: "a cosignature/v1 verified by formats/note.NewVerifierForCosignatureV1"}
			v.In("checkpoint_b64", c.signed).In("witness", w.VKey())
			v.Out("line", line).Out("timestamp", cosignTime).Out("cosigned_b64", cosigned)
			out.Write(fmt.Sprintf("witness/cosignatures/%s-%s.txt", strings.TrimPrefix(w.KeyName, "witness.example/"), c.name), v)
		}
	}
}

// ── Policies ─────────────────────────────────────────────────────────────

func policies() {
	other3 := &vec.Cosigner{KeyName: "witness.example/x3", Key: vec.Ed25519(13), Time: cosignTime}
	y1 := &vec.Cosigner{KeyName: "witness.example/y1", Key: vec.Ed25519(14), Time: cosignTime}
	y2 := &vec.Cosigner{KeyName: "witness.example/y2", Key: vec.Ed25519(15), Time: cosignTime}
	y3 := &vec.Cosigner{KeyName: "witness.example/y3", Key: vec.Ed25519(16), Time: cosignTime}
	stranger := &vec.Cosigner{KeyName: "witness.example/stranger", Key: vec.Ed25519(17), Time: cosignTime}
	log := "log " + logKey.VKey() + "\n"
	otherLog := vec.LogSigner{KeyName: "other.example/log", Key: vec.Ed25519(7)}
	// The log's own Ed25519 key presented as a witness (0x04).
	sock := &vec.Cosigner{KeyName: "witness.example/independent", Key: vec.Ed25519(7), Time: cosignTime}
	// A valid Ed25519 point whose 0x04 key ID under w1's name collides with
	// w1's (ground for audit round 3, A3-T3).
	ground := must(hex.DecodeString("9e8f351f000000005a0000000000000000000000000000000300000000000000"))
	shadow := vec.VKey(w1.KeyName, append([]byte{0x04}, ground...))
	if !strings.HasPrefix(shadow, strings.SplitN(w1.VKey(), "+", 3)[0]+"+"+strings.SplitN(w1.VKey(), "+", 3)[1]+"+") {
		panic("the ground key ID does not collide with w1's")
	}
	texts := map[string]string{
		"log-only":   log + "quorum none\n",
		"one-of-one": log + "witness w1 " + w1.VKey() + "\nquorum w1\n",
		"two-of-three": log + "witness w1 " + w1.VKey() + "\nwitness w2 " + w2.VKey() + "\nwitness w3 " + w3.VKey() +
			"\ngroup g 2 w1 w2 w3\nquorum g\n",
		"nested": "# Two operators' witnesses, both needed.\n" + log + "\n" +
			"witness X1 " + w1.VKey() + " https://x1.example/\n\twitness X2 " + w2.VKey() + "\nwitness X3 " + other3.VKey() +
			"\ngroup X-witnesses 2 X1 X2 X3\n\nwitness Y1 " + y1.VKey() + "\nwitness Y2 " + y2.VKey() + "\nwitness Y3 " + y3.VKey() +
			"\ngroup Y-witnesses any Y1 Y2 Y3\n  # comment\ngroup X-and-Y all X-witnesses Y-witnesses\nquorum X-and-Y\n",
		"duplicate-key":     log + "witness w1 " + w1.VKey() + "\nwitness w1b " + w1.VKey() + "\nquorum w1\n",
		"forward-reference": log + "group g any w1\nwitness w1 " + w1.VKey() + "\nquorum g\n",
		"k-above-n":         log + "witness w1 " + w1.VKey() + "\nwitness w2 " + w2.VKey() + "\ngroup g 3 w1 w2\nquorum g\n",
		"ml-dsa":            log + "witness w1 " + w1.VKey() + "\nwitness m1 " + ml.VKey() + "\ngroup g 2 w1 m1\nquorum g\n",
		"other-log":         "log " + otherLog.VKey() + "\nquorum none\n",
		"witness-is-log":    log + "witness w " + sock.VKey() + "\nquorum w\n",
		"ambiguous-key-id":  log + "witness attacker " + shadow + "\nwitness w1 " + w1.VKey() + "\nquorum w1\n",
	}
	// Policies formats accepts and the crate refuses (audit round 3).
	divergent := map[string][2]string{
		"witness-is-log": {"PolicyRule:WitnessIsLog",
			"the crate refuses a witness key that is also the log's key (signed-note: no key reuse across types); formats accepts it, and the log alone then meets the quorum"},
		"ambiguous-key-id": {"PolicyRule:AmbiguousKey",
			"the crate refuses two keys of one (name, key ID) when parsing; formats accepts the policy and refuses each checkpoint that names the pair (VerifierList ambiguity)"},
	}
	crate := map[string]string{
		"duplicate-key": "PolicyRule:DuplicateKey", "forward-reference": "PolicyRule:Undefined",
		"k-above-n": "PolicyRule:Threshold",
	}
	parsed := map[string]policy.TLogPolicy{}
	for _, name := range []string{"log-only", "one-of-one", "two-of-three", "nested", "duplicate-key", "forward-reference", "k-above-n", "ml-dsa", "other-log", "witness-is-log", "ambiguous-key-id"} {
		var p policy.TLogPolicy
		err := p.Unmarshal([]byte(texts[name]))
		v := &vec.Vector{Comment: "a tlog-policy parsed by formats/policy.Unmarshal"}
		v.In("policy_b64", []byte(texts[name])).Out("go", verdict(err))
		if d, ok := divergent[name]; ok {
			if err != nil {
				panic(fmt.Sprintf("formats refuses %s: %v", name, err))
			}
			v.Out("marshal_b64", p.Marshal()).Out("crate", d[0]).Out("divergence", d[1])
			// What formats then decides on a checkpoint cosigned by the
			// witness the policy names.
			var cosigned []byte
			if name == "witness-is-log" {
				cosigned = cosign(cp13, sock)
			} else {
				cosigned = cosign(cp13, w1)
			}
			_, verr := p.Verify(cosigned)
			v.Out("go_verify_cosigned", verdict(verr))
		} else if err == nil {
			parsed[name] = p
			v.Out("marshal_b64", p.Marshal()).Out("crate", "accept")
		} else {
			v.Out("crate", crate[name])
		}
		out.Write("policies/"+name+".txt", v)
	}

	tampered := func(signed []byte) []byte {
		// Flip one bit in the last signature line's signature bytes.
		s := strings.TrimSuffix(string(signed), "\n")
		at := strings.LastIndex(s, " ")
		raw := must(base64.StdEncoding.DecodeString(s[at+1:]))
		raw[len(raw)-1] ^= 1
		return []byte(s[:at+1] + b64(raw) + "\n")
	}
	wrongOrigin := sign(text(13, tree.root(13)), otherLog)
	cases := []struct {
		name, policy, crate string
		checkpoint          []byte
	}{
		{"log-only-signed", "log-only", "accept", cp13},
		{"log-only-other-key", "log-only", "NoTrustedSignature", sign(text(13, tree.root(13)), other)},
		{"log-only-wrong-origin", "log-only", "NoTrustedSignature", wrongOrigin},
		{"one-of-one-cosigned", "one-of-one", "accept", cosign(cp13, w1)},
		{"one-of-one-uncosigned", "one-of-one", "QuorumNotMet", cp13},
		{"one-of-one-invalid-cosignature", "one-of-one", "BadSignature", tampered(cosign(cp13, w1))},
		{"one-of-one-plus-unknown", "one-of-one", "accept", cosign(cp13, w1, stranger)},
		{"one-of-one-extensions", "one-of-one", "accept", cosign(cp13ext, w1)},
		{"two-of-three-two", "two-of-three", "accept", cosign(cp13, w1, w3)},
		{"two-of-three-one", "two-of-three", "QuorumNotMet", cosign(cp13, w2)},
		{"two-of-three-two-and-invalid", "two-of-three", "BadSignature", tampered(cosign(cp13, w1, w2, w3))},
		{"two-of-three-all", "two-of-three", "accept", cosign(cp13, w1, w2, w3)},
		{"two-of-three-strangers", "two-of-three", "QuorumNotMet", cosign(cp13, stranger, other3)},
		{"nested-met", "nested", "accept", cosign(cp13, w1, w2, y3)},
		{"nested-unmet", "nested", "QuorumNotMet", cosign(cp13, w1, y1)},
		{"ml-dsa-counted-by-go", "ml-dsa", "QuorumNotMet", cosign(cp13, w1, ml)},
		{"other-log-wrong-origin", "other-log", "OriginNotSigned", wrongOrigin},
	}
	for _, c := range cases {
		p := parsed[c.policy]
		_, err := p.Verify(c.checkpoint)
		if c.policy == "ml-dsa" && err != nil {
			panic(fmt.Sprintf("formats does not accept the ML-DSA-44 cosignature: %v", err))
		}
		v := &vec.Vector{Comment: "a checkpoint evaluated by formats/policy.TLogPolicy.Verify"}
		v.In("policy", c.policy).In("checkpoint_b64", c.checkpoint)
		v.Out("go", verdict(err)).Out("crate", c.crate)
		if c.policy == "ml-dsa" {
			v.Out("divergence", "the crate never counts ML-DSA-44 cosignatures (fail closed); formats verifies them")
		}
		out.Write("policies/verdicts/"+c.name+".txt", v)
	}
}

// ── Bundles ──────────────────────────────────────────────────────────────

func bundles() {
	cosigned := cosign(cp13, w1)
	path := tree.inclusion(5, 13)
	hashes := make([][sha256.Size]byte, len(path))
	for i, h := range path {
		copy(hashes[i][:], h)
	}
	for _, c := range []struct {
		name  string
		extra []byte
	}{
		{"index-5-extra", []byte("leaf-5")},
		{"index-5-no-extra", nil},
		{"index-5-long-extra", bytes.Repeat([]byte("x"), 49153)},
	} {
		b := proof.TLogProof{Index: 5, Hashes: hashes, Checkpoint: cosigned, ExtraData: c.extra}.Marshal()
		var back proof.TLogProof
		err := back.Unmarshal(b)
		v := &vec.Vector{Comment: "a tlog-proof bundle by formats/proof.TLogProof.Marshal"}
		v.In("index", 5).In("size", 13).In("extra_b64", c.extra)
		v.Out("bundle_b64", b).Out("go_unmarshal", verdict(err))
		if err == nil {
			leaf := rfc6962.DefaultHasher.HashLeaf([]byte("leaf-5"))
			v.Out("go_inclusion", verdict(m_proof.VerifyInclusion(rfc6962.DefaultHasher, 5, 13, leaf, path, tree.root(13))))
		}
		out.Write("bundles/"+c.name+".txt", v)
	}
}

// ── Tiles ────────────────────────────────────────────────────────────────

func tilePath(t tlog.Tile) string {
	return strings.Replace(t.Path(), "tile/8/", "tile/", 1)
}

func tiles() {
	for _, size := range []int64{1, 255, 256, 257, 65536, bigSize} {
		v := &vec.Vector{Comment: "tlog-tiles of the tree of leaves leaf-i (tlog.NewTiles, tlog.ReadTileData; height element removed)"}
		v.In("size", size)
		var all, half []string
		for _, t := range tlog.NewTiles(8, 0, size) {
			all = append(all, tilePath(t))
		}
		for _, t := range tlog.NewTiles(8, size/2, size) {
			half = append(half, tilePath(t))
		}
		v.Out("changed", strings.Join(all, " ")).Out("changed_half", strings.Join(half, " "))
		for _, t := range tlog.NewTiles(8, 0, size) {
			data := must(tlog.ReadTileData(t, tree))
			v.Out("tile", fmt.Sprintf("%s %x", tilePath(t), sha256.Sum256(data)))
			if t.L == 0 {
				var bundle []byte
				for i := int64(0); i < int64(t.W); i++ {
					leaf := []byte(fmt.Sprintf("leaf-%d", t.N*256+i))
					bundle = binary.BigEndian.AppendUint16(bundle, uint16(len(leaf)))
					bundle = append(bundle, leaf...)
					h := tlog.RecordHash(leaf)
					if !bytes.Equal(h[:], data[i*32:(i+1)*32]) {
						panic("entry does not hash to its tile")
					}
				}
				entries := strings.Replace(tilePath(t), "tile/0/", "tile/entries/", 1)
				v.Out("entries", fmt.Sprintf("%s %x", entries, sha256.Sum256(bundle)))
			}
		}
		out.Write(fmt.Sprintf("tiles/size-%d.txt", size), v)
	}
}

func main() {
	root := flag.String("out", "../../tests/vectors/c2sp", "where to write the vectors")
	flag.Parse()
	out = vec.NewWriter(*root)
	notes()
	keys()
	trees()
	witnessVectors()
	policies()
	bundles()
	tiles()
	out.Manifest()
	fmt.Println("vectors written to", *root)
}
