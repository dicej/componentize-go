package main

import (
	"archive/tar"
	"compress/gzip"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"

	"github.com/apparentlymart/go-userdirs/userdirs"
	"github.com/gofrs/flock"
)

func main() {
	directories := userdirs.ForApp("componentize-go", "dicej", "com.github.dicej-componentize-go")
	binDirectory := filepath.Join(directories.CacheDir, "bin")
	lockFilePath := filepath.Join(directories.CacheDir, "lock")
	binaryPath := filepath.Join(binDirectory, "componentize-go")

	if err := os.MkdirAll(binDirectory, 0755); err != nil {
		log.Fatalf("unable to create directory `%v`: %v", binDirectory, err)
	}

	lockFile := flock.New(lockFilePath)
	if err := lockFile.Lock(); err != nil {
		log.Fatalf("unable to lock file `%v`: %v", lockFilePath, err)
	}
	defer lockFile.Unlock()

	if _, err := os.Stat(binaryPath); errors.Is(err, os.ErrNotExist) {
		base := "https://github.com/dicej/componentize-go/releases/download/canary"
		url := fmt.Sprintf("%v/componentize-go-%v-%v.tar.gz", base, runtime.GOOS, runtime.GOARCH)

		fmt.Printf("Downloading `componentize-go` binary from %v and extracting to %v\n", url, binDirectory)

		response, err := http.Get(url)
		if err != nil {
			log.Fatalf("unable to download URL `%v`: %v", url, err)
		}
		defer response.Body.Close()

		uncompressed, err := gzip.NewReader(response.Body)
		if err != nil {
			log.Fatalf("unable to decompress content of URL `%v`: %v", url, err)
		}
		defer uncompressed.Close()

		untarred := tar.NewReader(uncompressed)
		for {
			header, err := untarred.Next()
			if err == io.EOF {
				break
			} else if err != nil {
				log.Fatalf("unable to untar content of URL `%v`: %v", url, err)
			}
			path := filepath.Join(binDirectory, header.Name)
			file, err := os.Create(path)
			if err != nil {
				log.Fatalf("unable to create file `%v`: %v", path, err)
			}
			if _, err := io.Copy(file, untarred); err != nil {
				log.Fatalf("unable to untar content of URL `%v`: %v", url, err)
			}
			file.Close()
		}

		if err := os.Chmod(binaryPath, 0755); err != nil {
			log.Fatalf("unable to make file `%v` executable: %v", binaryPath, err)
		}
	}

	command := exec.Command(binaryPath, os.Args[1:]...)

	stderr, err := command.StderrPipe()
	if err != nil {
		log.Fatalf("unable to get stderr for `%v` command: %v", binaryPath, err)
	}

	go func() {
		defer stderr.Close()
		io.Copy(os.Stderr, stderr)
	}()

	stdout, err := command.StdoutPipe()
	if err != nil {
		log.Fatalf("unable to get stdout for `%v` command: %v", binaryPath, err)
	}

	go func() {
		defer stdout.Close()
		io.Copy(os.Stdout, stdout)
	}()

	if err := command.Start(); err != nil {
		log.Fatalf("unable to start `%v` command: %v", binaryPath, err)
	}

	if err := command.Wait(); err != nil {
		if exiterr, ok := err.(*exec.ExitError); ok {
			code := exiterr.ExitCode()
			if code != 0 {
				os.Exit(code)
			}
		} else {
			log.Fatalf("trouble running `%v` command: %v", binaryPath, err)
		}
	}
}
