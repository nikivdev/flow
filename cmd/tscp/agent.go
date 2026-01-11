package main

import (
	"net"

	"golang.org/x/crypto/ssh/agent"
)

func dialAgent(socket string) (agent.ExtendedAgent, error) {
	conn, err := net.Dial("unix", socket)
	if err != nil {
		return nil, err
	}
	return agent.NewClient(conn), nil
}
