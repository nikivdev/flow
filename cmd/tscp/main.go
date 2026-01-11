package main

import (
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/pkg/sftp"
	"golang.org/x/crypto/ssh"
)

func main() {
	var (
		src       = flag.String("src", "", "Source file path")
		dst       = flag.String("dst", "", "Destination file path on remote machine")
		machine   = flag.String("machine", "", "Target machine name in tailnet")
		overwrite = flag.Bool("overwrite", false, "Overwrite existing file on remote")
		user      = flag.String("user", "", "SSH user on remote machine (defaults to current user)")
	)
	flag.Parse()

	if *src == "" || *dst == "" || *machine == "" {
		fmt.Fprintf(os.Stderr, "Usage: tscp -src <file> -dst <remote-path> -machine <name> [-overwrite] [-user <name>]\n")
		fmt.Fprintf(os.Stderr, "\nExample:\n")
		fmt.Fprintf(os.Stderr, "  tscp -src ~/bin/f -dst ~/bin/f -machine macbook -overwrite\n")
		os.Exit(1)
	}

	// Expand ~ in source path
	srcPath := expandPath(*src)

	// Get current user if not specified
	sshUser := *user
	if sshUser == "" {
		sshUser = os.Getenv("USER")
	}

	// Build tailnet hostname (append tailnet suffix if not present)
	host := *machine
	if !strings.Contains(host, ".") {
		// Tailscale MagicDNS: machine names are directly resolvable
		// No suffix needed if MagicDNS is enabled
	}

	if err := copyFile(srcPath, *dst, host, sshUser, *overwrite); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	fmt.Printf("Successfully copied %s to %s:%s\n", srcPath, host, *dst)
}

func expandPath(path string) string {
	if strings.HasPrefix(path, "~/") {
		home, _ := os.UserHomeDir()
		return filepath.Join(home, path[2:])
	}
	return path
}

func copyFile(src, dst, host, user string, overwrite bool) error {
	// Read source file
	srcFile, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open source: %w", err)
	}
	defer srcFile.Close()

	srcInfo, err := srcFile.Stat()
	if err != nil {
		return fmt.Errorf("stat source: %w", err)
	}

	// Connect via SSH using Tailscale SSH (uses ssh-agent or keys)
	config := &ssh.ClientConfig{
		User: user,
		Auth: []ssh.AuthMethod{
			ssh.PublicKeysCallback(sshAgent),
		},
		HostKeyCallback: ssh.InsecureIgnoreHostKey(), // Tailscale handles auth
	}

	client, err := ssh.Dial("tcp", host+":22", config)
	if err != nil {
		return fmt.Errorf("ssh connect to %s: %w", host, err)
	}
	defer client.Close()

	// Create SFTP client
	sftpClient, err := sftp.NewClient(client)
	if err != nil {
		return fmt.Errorf("sftp client: %w", err)
	}
	defer sftpClient.Close()

	// Expand ~ on remote side
	remotePath := dst
	if strings.HasPrefix(remotePath, "~/") {
		// Get remote home directory
		session, err := client.NewSession()
		if err == nil {
			out, err := session.Output("echo $HOME")
			if err == nil {
				home := strings.TrimSpace(string(out))
				remotePath = filepath.Join(home, dst[2:])
			}
			session.Close()
		}
	}

	// Check if file exists
	if _, err := sftpClient.Stat(remotePath); err == nil {
		if !overwrite {
			return fmt.Errorf("file %s already exists on %s (use -overwrite to replace)", remotePath, host)
		}
	}

	// Create parent directory if needed
	remoteDir := filepath.Dir(remotePath)
	sftpClient.MkdirAll(remoteDir)

	// Create/overwrite remote file
	dstFile, err := sftpClient.Create(remotePath)
	if err != nil {
		return fmt.Errorf("create remote file: %w", err)
	}
	defer dstFile.Close()

	// Copy contents
	_, err = io.Copy(dstFile, srcFile)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	// Set permissions (preserve from source)
	if err := sftpClient.Chmod(remotePath, srcInfo.Mode()); err != nil {
		return fmt.Errorf("chmod: %w", err)
	}

	return nil
}

func sshAgent() ([]ssh.Signer, error) {
	// Try to use SSH agent
	socket := os.Getenv("SSH_AUTH_SOCK")
	if socket == "" {
		return nil, fmt.Errorf("SSH_AUTH_SOCK not set - ensure ssh-agent is running")
	}

	conn, err := dialAgent(socket)
	if err != nil {
		return nil, err
	}

	return conn.Signers()
}
