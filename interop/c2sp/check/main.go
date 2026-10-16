// Command check verifies, with the Go reference implementations, what the
// crate emitted to target/interop/ (`cargo test --test transparency_interop
// -- --ignored emit`), and runs the stock transparency-dev witness on the
// crate's add-checkpoint requests. It writes the witness's replies back
// (replies/, witness.txt) for the crate's `witness_roundtrip` test.
//
//	go run ./check [-dir ../../target/interop]
package main

import (
	"bytes"
	"encoding/binary"
	"flag"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
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

const origin = "example.com/ledgers/acme"

var (
	dir    string
	w1     = &vec.Cosigner{KeyName: "witness.example/w1", Key: vec.Ed25519(9), Time: 1792108800}
	checks int
)

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func ok(err error, what string, args ...any) {
	if err != nil {
		panic(fmt.Sprintf(what+": %v", append(args, err)...))
	}
	checks++
}

func read(path string) []byte { return must(os.ReadFile(filepath.Join(dir, path))) }

func write(path string, data []byte) {
	full := filepath.Join(dir, path)
	must(0, os.MkdirAll(filepath.Dir(full), 0o755))
	must(0, os.WriteFile(full, data, 0o644))
}

func files(sub string) []string {
	matches := must(filepath.Glob(filepath.Join(dir, sub)))
	sort.Strings(matches)
	return matches
}

func main() {
	flag.StringVar(&dir, "dir", "../../target/interop", "the crate's emitted artefacts")
	flag.Parse()
	vkey := strings.TrimSpace(string(read("vkey.txt")))
	logVerifier := must(note.NewVerifier(vkey))

	// Checkpoints: note.Open and formats' ParseCheckpoint accept every one.
	var checkpoints [][]byte
	for _, path := range files("checkpoints/*.note") {
		cp := must(os.ReadFile(path))
		_, err := note.Open(cp, note.VerifierList(logVerifier))
		ok(err, "note.Open %s", path)
		_, _, _, err = f_log.ParseCheckpoint(cp, origin, logVerifier)
		ok(err, "ParseCheckpoint %s", path)
		checkpoints = append(checkpoints, cp)
	}
	if len(checkpoints) != 10 {
		panic(fmt.Sprintf("%d checkpoints, 10 expected", len(checkpoints)))
	}

	// The stock witness: the ten requests in order, then the refusals.
	w := vec.NewWitness(w1, vkey)
	defer w.Close()
	write("witness.txt", []byte(w1.VKey()+"\n"))
	cosigVerifier := must(f_note.NewVerifierForCosignatureV1(w1.VKey()))
	for i, path := range files("requests/*.body") {
		r := w.Post(must(os.ReadFile(path)))
		if r.Status != 200 {
			panic(fmt.Sprintf("%s: status %d", path, r.Status))
		}
		n, err := note.Open(append(append([]byte{}, checkpoints[i]...), r.Body...), note.VerifierList(logVerifier, cosigVerifier))
		ok(err, "the cosigned checkpoint %d", i)
		if len(n.Sigs) != 2 {
			panic("the witness's cosignature does not verify")
		}
		write(fmt.Sprintf("replies/%02d.body", i), r.Body)
		checks++
	}
	last := checkpoints[len(checkpoints)-1]
	lastCP, _, _, err := f_log.ParseCheckpoint(last, origin, logVerifier)
	ok(err, "the last checkpoint")
	for _, c := range []struct {
		path   string
		status int
		body   string
	}{
		{"stale.body", 409, fmt.Sprintf("%d\n", lastCP.Size)},
		{"foreign.body", 403, ""},
		{"corrupt.body", 422, ""},
	} {
		r := w.Post(read(c.path))
		if r.Status != c.status || string(r.Body) != c.body {
			panic(fmt.Sprintf("%s: %d %q, expected %d %q", c.path, r.Status, r.Body, c.status, c.body))
		}
		if c.status == 409 && r.ContentType != "text/x.tlog.size" {
			panic("409 without text/x.tlog.size")
		}
		checks++
	}
	cosigned := append(append([]byte{}, last...), must(os.ReadFile(filepath.Join(dir, fmt.Sprintf("replies/%02d.body", len(checkpoints)-1))))...)

	// Policies the crate rendered: formats parses them, writes the same
	// bytes, and accepts the (cosigned) last checkpoint.
	for _, path := range files("policies/*.txt") {
		text := must(os.ReadFile(path))
		var p policy.TLogPolicy
		ok(p.Unmarshal(text), "policy %s", path)
		if !bytes.Equal(p.Marshal(), text) {
			panic(fmt.Sprintf("%s: formats writes %q", path, p.Marshal()))
		}
		_, err := p.Verify(cosigned)
		ok(err, "policy %s verifies the cosigned checkpoint", path)
	}

	// Bundles: formats/proof parses them; merkle/proof verifies every path
	// against the documented layout.
	for _, path := range files("bundles/*.tlog-proof") {
		var b proof.TLogProof
		ok(b.Unmarshal(must(os.ReadFile(path))), "proof.Unmarshal %s", path)
		cp, _, _, err := f_log.ParseCheckpoint(b.Checkpoint, origin, logVerifier)
		ok(err, "the bundle's checkpoint %s", path)
		hashes := make([][]byte, len(b.Hashes))
		for i := range b.Hashes {
			hashes[i] = b.Hashes[i][:]
		}
		if strings.Contains(path, "balance") {
			checkBalance(b, cp, hashes, path)
			continue
		}
		leaf := rfc6962.DefaultHasher.HashLeaf(b.ExtraData)
		ok(m_proof.VerifyInclusion(rfc6962.DefaultHasher, b.Index, cp.Size, leaf, hashes, cp.Hash), "inclusion %s", path)
	}

	// Tiles: rebuilt from the entry bundles with sumdb/tlog, byte for byte.
	checkTiles(lastCP)
	fmt.Printf("check: %d checks passed (checkpoints, stock witness, policies, bundles, tiles)\n", checks)
}

// reader reads the canonical encoding of the crate's records.
type reader struct{ b []byte }

func (r *reader) take(n int) []byte {
	if len(r.b) < n {
		panic("truncated")
	}
	out := r.b[:n]
	r.b = r.b[n:]
	return out
}
func (r *reader) u8() byte    { return r.take(1)[0] }
func (r *reader) u32() uint32 { return binary.LittleEndian.Uint32(r.take(4)) }
func (r *reader) u64() uint64 { return binary.LittleEndian.Uint64(r.take(8)) }
func (r *reader) bytes() []byte {
	return r.take(int(r.u64()))
}
func (r *reader) hashes() [][]byte {
	n := int(r.u64())
	out := make([][]byte, n)
	for i := range out {
		out[i] = r.take(32)
	}
	return out
}

// checkBalance verifies the three RFC 6962 paths of a sealed-balance
// bundle (module docs of the crate's transparency::bundle): the seal at
// `index`, the balance leaf at the handle in the seal's tree, and the
// account registration.
func checkBalance(b proof.TLogProof, cp *f_log.Checkpoint, sealPath [][]byte, path string) {
	h := rfc6962.DefaultHasher
	r := &reader{b.ExtraData}
	if tag := string(r.bytes()); tag != "doubleentry/balance-proof/1" {
		panic("tag " + tag)
	}
	sealLeaf := r.bytes()
	currency := string(r.bytes())
	layer := r.u8()
	handle := r.u32()
	var balanceLeaf []byte
	var balancePath [][]byte
	if r.u8() == 1 {
		balanceLeaf = r.bytes()
		balancePath = r.hashes()
	}
	accountPos := r.u64()
	accountLeaf := r.bytes()
	accountPath := r.hashes()
	if len(r.b) != 0 {
		panic("trailing bytes")
	}
	ok(m_proof.VerifyInclusion(h, b.Index, cp.Size, h.HashLeaf(sealLeaf), sealPath, cp.Hash), "seal inclusion %s", path)
	ok(m_proof.VerifyInclusion(h, accountPos, cp.Size, h.HashLeaf(accountLeaf), accountPath, cp.Hash), "account inclusion %s", path)
	if accountLeaf[0] != 0x02 || accountPos >= b.Index {
		panic("the account record is not a registration before the seal")
	}
	// The seal record: kind, period, start, end, through, accounts, trees.
	s := &reader{sealLeaf}
	if s.u8() != 0x08 {
		panic("not a seal")
	}
	s.bytes()
	s.take(8)
	s.u64()
	accounts := s.u32()
	trees := int(s.u64())
	for i := 0; i < trees; i++ {
		c, l, root := string(s.bytes()), s.u8(), s.take(32)
		s.take(40)
		if c == currency && l == layer {
			ok(m_proof.VerifyInclusion(h, uint64(handle), uint64(accounts), h.HashLeaf(balanceLeaf), balancePath, root),
				"balance inclusion %s", path)
			return
		}
	}
	if balanceLeaf != nil {
		panic("a balance path for a tree the seal does not have")
	}
}

func checkTiles(cp *f_log.Checkpoint) {
	var leaves [][]byte
	var bundles []string
	must(0, filepath.WalkDir(filepath.Join(dir, "tile/entries"), func(path string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			bundles = append(bundles, path)
		}
		return err
	}))
	type indexed struct {
		n    int64
		data []byte
	}
	var ordered []indexed
	for _, path := range bundles {
		rel := strings.TrimPrefix(filepath.ToSlash(must(filepath.Rel(dir, path))), "tile/entries/")
		t := must(tlog.ParseTilePath("tile/8/data/" + rel))
		ordered = append(ordered, indexed{t.N, must(os.ReadFile(path))})
	}
	sort.Slice(ordered, func(i, j int) bool { return ordered[i].n < ordered[j].n })
	for _, bundle := range ordered {
		r := bundle.data
		for len(r) > 0 {
			n := int(binary.BigEndian.Uint16(r))
			leaves = append(leaves, r[2:2+n])
			r = r[2+n:]
		}
	}
	if uint64(len(leaves)) != cp.Size {
		panic(fmt.Sprintf("%d leaves in the entry bundles, checkpoint size %d", len(leaves), cp.Size))
	}
	var stored []tlog.Hash
	reader := tlog.HashReaderFunc(func(indexes []int64) ([]tlog.Hash, error) {
		out := make([]tlog.Hash, len(indexes))
		for i, x := range indexes {
			out[i] = stored[x]
		}
		return out, nil
	})
	for i, leaf := range leaves {
		stored = append(stored, must(tlog.StoredHashes(int64(i), leaf, reader))...)
	}
	root := must(tlog.TreeHash(int64(cp.Size), reader))
	if !bytes.Equal(root[:], cp.Hash) {
		panic("the entry bundles do not hash to the checkpoint's root")
	}
	checks++
	want := map[string]bool{}
	for _, t := range tlog.NewTiles(8, 0, int64(cp.Size)) {
		want[strings.Replace(t.Path(), "tile/8/", "tile/", 1)] = true
	}
	found := 0
	must(0, filepath.WalkDir(filepath.Join(dir, "tile"), func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || strings.Contains(filepath.ToSlash(path), "/tile/entries/") {
			return err
		}
		rel := filepath.ToSlash(must(filepath.Rel(dir, path)))
		if !want[rel] {
			panic("an unexpected tile " + rel)
		}
		t := must(tlog.ParseTilePath(strings.Replace(rel, "tile/", "tile/8/", 1)))
		if !bytes.Equal(must(tlog.ReadTileData(t, reader)), must(os.ReadFile(path))) {
			panic("tile bytes differ: " + rel)
		}
		found++
		checks++
		return nil
	}))
	if found != len(want) {
		panic(fmt.Sprintf("%d tiles, %d expected", found, len(want)))
	}
}
