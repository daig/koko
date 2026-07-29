use super::*;

impl Parser {
    pub(super) fn statement(&mut self) -> Result<Statement> {
        if self.at_kw("EXPLAIN") || self.at_kw("PROFILE") {
            let profile = self.at_kw("PROFILE");
            self.advance();
            if !profile {
                self.eat_kw("LOGICAL");
            }
            let inner = Box::new(self.statement()?);
            return Ok(Statement::Explain { inner, profile });
        }
        if self.at_kw("CREATE") {
            if self.at_kw_ahead(1, "GRAPH") {
                return self.create_graph().map(Statement::CreateGraph);
            }
            if self.at_kw_ahead(1, "INDEX")
                || (self.at_kw_ahead(1, "HASH") && self.at_kw_ahead(2, "INDEX"))
                || (self.at_kw_ahead(1, "ART") && self.at_kw_ahead(2, "INDEX"))
            {
                return self.create_index().map(Statement::CreateIndex);
            }
            if self.at_kw_ahead(1, "NODE") && self.at_kw_ahead(2, "TABLE") {
                return self.create_node_table();
            }
            if self.at_kw_ahead(1, "REL") && self.at_kw_ahead(2, "TABLE") {
                return self.create_rel_table();
            }
            if self.at_kw_ahead(1, "SEQUENCE") {
                return self.create_sequence().map(Statement::CreateSequence);
            }
            if self.at_kw_ahead(1, "TYPE") {
                return self.create_type().map(Statement::CreateType);
            }
            if self.at_kw_ahead(1, "MACRO") {
                return self.create_macro().map(Statement::CreateMacro);
            }
        }
        if self.at_kw("USE") && self.at_kw_ahead(1, "GRAPH") {
            self.advance();
            self.advance();
            return Ok(Statement::UseGraph {
                name: self.ident()?,
            });
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "GRAPH") {
            self.advance();
            self.advance();
            let if_exists = self.parse_if_exists();
            return Ok(Statement::DropGraph {
                name: self.ident()?,
                if_exists,
            });
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "INDEX") {
            self.advance();
            self.advance();
            let if_exists = self.parse_if_exists();
            return Ok(Statement::DropIndex(DropIndex {
                name: self.ident()?,
                if_exists,
            }));
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "TABLE") {
            return self.drop_table().map(Statement::DropTable);
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "SEQUENCE") {
            return self.drop_sequence().map(Statement::DropSequence);
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "MACRO") {
            return self.drop_macro();
        }
        if self.at_kw("COMMENT") && self.at_kw_ahead(1, "ON") {
            return self.comment_statement().map(Statement::Comment);
        }
        if self.at_kw("ALTER") && self.at_kw_ahead(1, "TABLE") {
            return self.alter_statement().map(Statement::Alter);
        }
        if self.at_kw("COPY") {
            if self.peek_at(1) == &Tok::LParen {
                return self.copy_to_statement().map(Statement::CopyTo);
            }
            return self.copy_statement().map(Statement::Copy);
        }
        if self.at_kw("EXPORT") {
            return self
                .export_database_statement()
                .map(Statement::ExportDatabase);
        }
        if self.at_kw("IMPORT") {
            return self
                .import_database_statement()
                .map(Statement::ImportDatabase);
        }
        if self.at_kw("BEGIN")
            || self.at_kw("COMMIT")
            || self.at_kw("ROLLBACK")
            || self.at_kw("CHECKPOINT")
        {
            return self.transaction_statement().map(Statement::Transaction);
        }
        if self.at_kw("CALL") {
            return self.call_statement();
        }
        self.regular_query().map(Statement::Query)
    }
}
