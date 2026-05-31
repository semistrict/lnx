package main

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/charmbracelet/lipgloss/table"
)

var (
	headerStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("15"))
	dimStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))
	greenStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("2"))
	cyanStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("6"))
	yellowStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("3"))
	labelStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))
	valueStyle  = lipgloss.NewStyle().Bold(true)
)

func newTable(headers ...string) *table.Table {
	return table.New().
		Headers(headers...).
		Border(lipgloss.NormalBorder()).
		BorderTop(false).
		BorderBottom(false).
		BorderLeft(false).
		BorderRight(false).
		BorderColumn(false).
		BorderHeader(true).
		StyleFunc(func(row, col int) lipgloss.Style {
			if row == table.HeaderRow {
				return headerStyle
			}
			return lipgloss.NewStyle().PaddingRight(2)
		})
}
