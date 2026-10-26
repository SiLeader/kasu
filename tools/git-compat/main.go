package main

import (
	"bytes"
	"fmt"
	git "github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/plumbing/filemode"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/go-git/go-git/v5/plumbing/transport/http"
	"github.com/go-git/go-git/v5/plumbing/transport/ssh"
	"github.com/go-git/go-git/v5/utils/trace"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

type packetLog struct{}

func (packetLog) Write(p []byte) (int, error) {
	msg := strings.TrimSuffix(string(p), "\n")
	if marker := bytes.Index(p, []byte("packet: < ")); marker >= 0 {
		offset := marker + len("packet: < ") + 5
		if offset < len(p) && p[offset] >= 1 && p[offset] <= 3 {
			msg = string(p[:offset]) + "[sideband payload omitted]"
		}
	}
	_, err := fmt.Fprintln(os.Stderr, strconv.QuoteToASCII(msg))
	return len(p), err
}
func main() {
	if len(os.Args) != 4 {
		panic("usage: git-compat <url> <destination> <depth>")
	}
	depth, err := strconv.Atoi(os.Args[3])
	if err != nil {
		panic(err)
	}
	trace.SetLogger(log.New(packetLog{}, "", 0))
	trace.SetTarget(trace.Packet)
	opts := &git.CloneOptions{URL: os.Args[1], Depth: depth, NoCheckout: false}
	if key := os.Getenv("KASU_SSH_KEY"); key != "" {
		user := os.Getenv("KASU_SSH_USER")
		if user == "" {
			user = "git"
		}
		auth, err := ssh.NewPublicKeysFromFile(user, key, "")
		if err != nil {
			panic(err)
		}
		auth.HostKeyCallback, err = ssh.NewKnownHostsCallback(os.Getenv("KASU_SSH_KNOWN_HOSTS"))
		if err != nil {
			panic(err)
		}
		opts.Auth = auth
	} else if token := os.Getenv("KASU_TOKEN"); token != "" {
		opts.Auth = &http.BasicAuth{Username: "git", Password: token}
	}
	repo, err := git.PlainClone(os.Args[2], false, opts)
	if err != nil {
		panic(err)
	}
	head, err := repo.Head()
	if err != nil {
		panic(err)
	}
	commit, err := repo.CommitObject(head.Hash())
	if err != nil {
		panic(err)
	}
	tree, err := commit.Tree()
	if err != nil {
		panic(err)
	}
	err = tree.Files().ForEach(func(file *object.File) error {
		expected, err := file.Contents()
		if err != nil {
			return err
		}
		path := filepath.Join(os.Args[2], filepath.FromSlash(file.Name))
		var actual []byte
		if file.Mode == filemode.Symlink {
			target, e := os.Readlink(path)
			if e != nil {
				return e
			}
			actual = []byte(target)
		} else {
			actual, err = os.ReadFile(path)
			if err != nil {
				return err
			}
			info, e := os.Stat(path)
			if e != nil {
				return e
			}
			if (file.Mode == filemode.Executable) != (info.Mode().Perm()&0111 != 0) {
				return fmt.Errorf("executable mode mismatch: %q", file.Name)
			}
		}
		if !bytes.Equal(actual, []byte(expected)) {
			return fmt.Errorf("checkout content mismatch: %q", file.Name)
		}
		return nil
	})
	if err != nil {
		panic(err)
	}
	result := head.Hash()
	if os.Getenv("KASU_PUSH") == "1" {
		worktree, err := repo.Worktree()
		if err != nil {
			panic(err)
		}
		payload := []byte(fmt.Sprintf("client: go-git\ndepth: %d\n", depth))
		if err := os.WriteFile(filepath.Join(os.Args[2], "go-git-writeback.yaml"), payload, 0644); err != nil {
			panic(err)
		}
		if _, err := worktree.Add("go-git-writeback.yaml"); err != nil {
			panic(err)
		}
		result, err = worktree.Commit("go-git write-back fixture", &git.CommitOptions{Author: &object.Signature{Name: "go-git fixture", Email: "e2e@kasu.invalid", When: time.Unix(1700000100, 0).UTC()}})
		if err != nil {
			panic(err)
		}
		if err := repo.Push(&git.PushOptions{Auth: opts.Auth}); err != nil {
			panic(err)
		}
	}
	fmt.Println(result.String())
}
