#!/usr/bin/env python3
"""
Flow CLI - Demonstrate swarms in action

Usage:
    flow single "Your question here"
    flow sequential "Research topic"
    flow concurrent "Analysis task"
    flow hierarchical "Complex project"
    flow rearrange "Creative task"
    flow chat "Discussion topic"
    flow auto "Task description"
"""

import argparse
import sys
from rich.console import Console
from rich.panel import Panel
from rich.markdown import Markdown

console = Console()


def demo_single(task: str, model: str = "gpt-4o-mini"):
    """Run a single agent on a task."""
    from swarms import Agent

    console.print(Panel(f"[bold cyan]Single Agent Demo[/bold cyan]\nTask: {task}"))

    agent = Agent(
        agent_name="Assistant",
        model_name=model,
        max_loops=1,
        streaming=True,
    )

    console.print("\n[yellow]Agent thinking...[/yellow]\n")
    response = agent.run(task)
    console.print(Panel(Markdown(response), title="[green]Response[/green]"))


def demo_sequential(task: str, model: str = "gpt-4o-mini"):
    """Run a sequential workflow: Researcher -> Analyst -> Writer."""
    from swarms import Agent, SequentialWorkflow

    console.print(Panel(f"[bold cyan]Sequential Workflow Demo[/bold cyan]\n"
                       f"Pipeline: Researcher -> Analyst -> Writer\n"
                       f"Task: {task}"))

    researcher = Agent(
        agent_name="Researcher",
        system_prompt="You are a thorough researcher. Investigate the topic and provide detailed findings with sources and data.",
        model_name=model,
        max_loops=1,
    )

    analyst = Agent(
        agent_name="Analyst",
        system_prompt="You are an analytical expert. Take the research provided and identify key patterns, insights, and implications.",
        model_name=model,
        max_loops=1,
    )

    writer = Agent(
        agent_name="Writer",
        system_prompt="You are a skilled writer. Take the analysis and create a clear, engaging summary with actionable conclusions.",
        model_name=model,
        max_loops=1,
    )

    workflow = SequentialWorkflow(agents=[researcher, analyst, writer])

    console.print("\n[yellow]Running pipeline...[/yellow]\n")
    result = workflow.run(task)
    console.print(Panel(Markdown(str(result)), title="[green]Final Output[/green]"))


def demo_concurrent(task: str, model: str = "gpt-4o-mini"):
    """Run agents concurrently with different perspectives."""
    from swarms import Agent, ConcurrentWorkflow

    console.print(Panel(f"[bold cyan]Concurrent Workflow Demo[/bold cyan]\n"
                       f"Running 3 agents in parallel with different perspectives\n"
                       f"Task: {task}"))

    optimist = Agent(
        agent_name="Optimist",
        system_prompt="You see opportunities and positive outcomes. Analyze from an optimistic perspective, highlighting benefits and potential.",
        model_name=model,
        max_loops=1,
    )

    critic = Agent(
        agent_name="Critic",
        system_prompt="You identify risks and challenges. Analyze from a critical perspective, highlighting potential problems and concerns.",
        model_name=model,
        max_loops=1,
    )

    pragmatist = Agent(
        agent_name="Pragmatist",
        system_prompt="You focus on practical implementation. Analyze from a pragmatic perspective, highlighting actionable steps and trade-offs.",
        model_name=model,
        max_loops=1,
    )

    workflow = ConcurrentWorkflow(agents=[optimist, critic, pragmatist])

    console.print("\n[yellow]Running agents in parallel...[/yellow]\n")
    results = workflow.run(task)

    for agent_name, output in results.items():
        console.print(Panel(Markdown(str(output)), title=f"[green]{agent_name}[/green]"))


def demo_hierarchical(task: str, model: str = "gpt-4o-mini"):
    """Run a hierarchical swarm with a director and workers."""
    from swarms import Agent, HierarchicalSwarm

    console.print(Panel(f"[bold cyan]Hierarchical Swarm Demo[/bold cyan]\n"
                       f"Director assigns tasks to specialized workers\n"
                       f"Task: {task}"))

    planner = Agent(
        agent_name="Planner",
        system_prompt="You create detailed project plans and break down complex tasks into actionable steps.",
        model_name=model,
        max_loops=1,
    )

    executor = Agent(
        agent_name="Executor",
        system_prompt="You implement plans and execute tasks efficiently, providing concrete outputs.",
        model_name=model,
        max_loops=1,
    )

    reviewer = Agent(
        agent_name="Reviewer",
        system_prompt="You review work for quality, completeness, and suggest improvements.",
        model_name=model,
        max_loops=1,
    )

    swarm = HierarchicalSwarm(
        name="Project-Team",
        description="A team that plans, executes, and reviews work",
        agents=[planner, executor, reviewer],
        max_loops=1,
    )

    console.print("\n[yellow]Swarm working...[/yellow]\n")
    result = swarm.run(task)
    console.print(Panel(Markdown(str(result)), title="[green]Team Output[/green]"))


def demo_rearrange(task: str, model: str = "gpt-4o-mini"):
    """Run agent rearrange with custom flow patterns."""
    from swarms import Agent, AgentRearrange

    console.print(Panel(f"[bold cyan]Agent Rearrange Demo[/bold cyan]\n"
                       f"Flow: idea -> designer, developer -> integrator\n"
                       f"Task: {task}"))

    idea = Agent(
        agent_name="idea",
        system_prompt="You generate creative ideas and concepts. Brainstorm possibilities.",
        model_name=model,
        max_loops=1,
    )

    designer = Agent(
        agent_name="designer",
        system_prompt="You design user experiences and visual concepts based on ideas provided.",
        model_name=model,
        max_loops=1,
    )

    developer = Agent(
        agent_name="developer",
        system_prompt="You think about technical implementation and architecture based on ideas provided.",
        model_name=model,
        max_loops=1,
    )

    integrator = Agent(
        agent_name="integrator",
        system_prompt="You synthesize design and development perspectives into a cohesive plan.",
        model_name=model,
        max_loops=1,
    )

    # idea sends to both designer and developer, both send to integrator
    flow = "idea -> designer, developer -> integrator"

    rearrange = AgentRearrange(
        agents=[idea, designer, developer, integrator],
        flow=flow,
    )

    console.print("\n[yellow]Agents coordinating...[/yellow]\n")
    result = rearrange.run(task)
    console.print(Panel(Markdown(str(result)), title="[green]Integrated Output[/green]"))


def demo_chat(topic: str, model: str = "gpt-4o-mini", rounds: int = 3):
    """Run a group chat discussion."""
    from swarms import Agent, GroupChat

    console.print(Panel(f"[bold cyan]Group Chat Demo[/bold cyan]\n"
                       f"3 experts discussing for {rounds} rounds\n"
                       f"Topic: {topic}"))

    scientist = Agent(
        agent_name="Scientist",
        system_prompt="You are a scientist who values evidence and empirical data. Contribute scientific perspectives to discussions.",
        model_name=model,
        max_loops=1,
    )

    philosopher = Agent(
        agent_name="Philosopher",
        system_prompt="You are a philosopher who explores ethical and conceptual dimensions. Contribute philosophical perspectives.",
        model_name=model,
        max_loops=1,
    )

    engineer = Agent(
        agent_name="Engineer",
        system_prompt="You are an engineer focused on practical solutions. Contribute engineering perspectives.",
        model_name=model,
        max_loops=1,
    )

    chat = GroupChat(
        name="Expert-Panel",
        description="A panel of experts discussing complex topics",
        agents=[scientist, philosopher, engineer],
        max_loops=rounds,
    )

    console.print("\n[yellow]Discussion starting...[/yellow]\n")
    result = chat.run(f"Let's discuss: {topic}")
    console.print(Panel(Markdown(str(result)), title="[green]Discussion Summary[/green]"))


def demo_auto(task: str, model: str = "gpt-4o-mini"):
    """Auto-generate a swarm for a task."""
    from swarms.structs.auto_swarm_builder import AutoSwarmBuilder
    import json

    console.print(Panel(f"[bold cyan]Auto Swarm Builder Demo[/bold cyan]\n"
                       f"Automatically generates specialized agents\n"
                       f"Task: {task}"))

    swarm = AutoSwarmBuilder(
        name="Auto-Generated-Swarm",
        description="Automatically built swarm for the task",
        verbose=True,
        max_loops=1,
        model_name=model,
    )

    console.print("\n[yellow]Building and running swarm...[/yellow]\n")
    result = swarm.run(task=task)

    if isinstance(result, dict):
        console.print(Panel(json.dumps(result, indent=2), title="[green]Swarm Output[/green]"))
    else:
        console.print(Panel(Markdown(str(result)), title="[green]Swarm Output[/green]"))


def main():
    parser = argparse.ArgumentParser(
        description="Flow CLI - Demonstrate swarms in action",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  flow single "What is quantum computing?"
  flow sequential "Research the future of renewable energy"
  flow concurrent "Analyze the pros and cons of remote work"
  flow hierarchical "Plan a mobile app launch strategy"
  flow rearrange "Design a new productivity tool"
  flow chat "The impact of AI on society"
  flow auto "Create a team to analyze cryptocurrency trends"
        """
    )

    parser.add_argument(
        "--model", "-m",
        default="gpt-4o-mini",
        help="Model to use (default: gpt-4o-mini)"
    )

    subparsers = parser.add_subparsers(dest="command", help="Demo type")

    # Single agent
    single_parser = subparsers.add_parser("single", help="Single agent demo")
    single_parser.add_argument("task", help="Task for the agent")

    # Sequential workflow
    seq_parser = subparsers.add_parser("sequential", help="Sequential workflow (Researcher -> Analyst -> Writer)")
    seq_parser.add_argument("task", help="Topic to research and write about")

    # Concurrent workflow
    conc_parser = subparsers.add_parser("concurrent", help="Concurrent agents (Optimist, Critic, Pragmatist)")
    conc_parser.add_argument("task", help="Task to analyze from multiple perspectives")

    # Hierarchical swarm
    hier_parser = subparsers.add_parser("hierarchical", help="Hierarchical swarm (Director with workers)")
    hier_parser.add_argument("task", help="Complex project to coordinate")

    # Agent rearrange
    rear_parser = subparsers.add_parser("rearrange", help="Agent rearrange with custom flow")
    rear_parser.add_argument("task", help="Creative task for the flow")

    # Group chat
    chat_parser = subparsers.add_parser("chat", help="Group chat discussion")
    chat_parser.add_argument("topic", help="Topic to discuss")
    chat_parser.add_argument("--rounds", "-r", type=int, default=3, help="Discussion rounds (default: 3)")

    # Auto swarm builder
    auto_parser = subparsers.add_parser("auto", help="Auto-generate a swarm")
    auto_parser.add_argument("task", help="Task description for auto-generated swarm")

    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        sys.exit(0)

    console.print(f"\n[dim]Using model: {args.model}[/dim]\n")

    try:
        if args.command == "single":
            demo_single(args.task, args.model)
        elif args.command == "sequential":
            demo_sequential(args.task, args.model)
        elif args.command == "concurrent":
            demo_concurrent(args.task, args.model)
        elif args.command == "hierarchical":
            demo_hierarchical(args.task, args.model)
        elif args.command == "rearrange":
            demo_rearrange(args.task, args.model)
        elif args.command == "chat":
            demo_chat(args.topic, args.model, args.rounds)
        elif args.command == "auto":
            demo_auto(args.task, args.model)
    except KeyboardInterrupt:
        console.print("\n[red]Interrupted[/red]")
        sys.exit(1)
    except Exception as e:
        console.print(f"\n[red]Error: {e}[/red]")
        sys.exit(1)


if __name__ == "__main__":
    main()
